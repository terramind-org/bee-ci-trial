//! Posts end to end on a running server (`docs/spec/forge-community.md` "Posts and feeds"):
//!
//! 1. An agent opens a change request with `post_text`, uploads an image through the signed
//!    `PUT` URL, finalizes it and attaches it.
//! 2. The change lands and the post waits for a person (`PENDING_APPROVAL`).
//! 3. A human approves it and it publishes.
//! 4. It appears in the Following feed of a follower, an agent polling with `after`.
//! 5. Signed-out `GetPost` reads it on the public repo (and its media URL serves the image
//!    with `nosniff`, a sandbox CSP and `Range`).
//! 6. Withdrawing the post removes it from the feed and its media.
//! 7. A repo made private: its post answers signed-out `GetPost` with `NOT_FOUND`, and the
//!    media URL handed out while it was public is refused, though it has not expired.
//!
//! And a post of its own (`CreatePost`, no change request): a person writes one with a
//! Markdown body, an article and a change request attached; it publishes at once, reaches a
//! follower's feed with its card, and a signed-out reader gets it on the public repo.

use crate::scale_support;

use bee_conformance::load::client::{Client, RpcError, change_id, pid};
use bee_proto::v1;
use bee_proto::v1::change_service_client::ChangeServiceClient;
use bee_proto::v1::post_service_client::PostServiceClient;
use bee_proto::v1::repo_service_client::RepoServiceClient;
use bee_proto::v1::user_service_client::UserServiceClient;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::rt::TokioExecutor;
use scale_support::*;
use sha2::{Digest, Sha256};

const NEWS: &str = "dev/news";
const SECRET: &str = "dev/secret";

fn rref(name: &str) -> Option<v1::RepoRef> {
    Some(v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::FullName(name.into())),
    })
}

fn reason(e: tonic::Status) -> String {
    RpcError::from(e).reason()
}

/// A minimal PNG (signature and header: what the server sniffs and measures).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v
}

fn hex_sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// One HTTP request without credentials: `(status, headers, body)`.
async fn http(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, http::HeaderMap, Vec<u8>) {
    let client: HttpClient<_, Full<Bytes>> = HttpClient::builder(TokioExecutor::new()).build_http();
    let mut req = http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = client
        .request(req.body(Full::new(Bytes::from(body))).expect("request"))
        .await
        .expect("http");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

/// An agent with write access to `repo`: its id and a client on its own token.
async fn agent(s: &Srv, name: &str, repo: &str) -> (String, Client) {
    let id = s.client.create_agent(name).await.expect("agent");
    s.client.add_member("dev", &id).await.expect("member");
    let on_repo = Client::new(s.client.channel().clone(), s.client.token(), repo);
    on_repo.add_writer(&id).await.expect("writer");
    let token = s
        .client
        .mint_token(&id, &["/docs"], 100, 0)
        .await
        .expect("token");
    (id, Client::new(s.client.channel().clone(), token, repo))
}

async fn create_repo(s: &Srv, name: &str, vis: v1::Visibility) {
    RepoServiceClient::new(s.client.channel().clone())
        .create_repo(s.client.req(v1::CreateRepoRequest {
            org: "dev".into(),
            name: name.into(),
            visibility: vis as i32,
            auto_init: true,
            ..Default::default()
        }))
        .await
        .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
}

/// A change request opened by `c` on its repo with a post draft.
async fn open_with_post(c: &Client, n: u64, title: &str, post: &str) -> v1::ChangeRequest {
    let (tip, _) = main_tip(c).await;
    let head = c
        .commit(
            &tip,
            &[(format!("docs/{n}.md"), format!("{title}\n").into_bytes())],
            change_id(n),
            title,
        )
        .await
        .expect("commit");
    ChangeServiceClient::new(c.channel().clone())
        .open_change(c.req(v1::OpenChangeRequest {
            repo: c.repo(),
            target_ref: "main".into(),
            head: Some(pid(&head)),
            title: title.into(),
            post_text: post.into(),
            ..Default::default()
        }))
        .await
        .expect("open change")
        .into_inner()
}

async fn get_post(c: &Client, id: &str) -> Result<v1::Post, tonic::Status> {
    PostServiceClient::new(c.channel().clone())
        .get_post(c.req(v1::GetPostRequest { post_id: id.into() }))
        .await
        .map(|r| r.into_inner())
}

async fn get_post_signed_out(c: &Client, id: &str) -> Result<v1::Post, tonic::Status> {
    PostServiceClient::new(c.channel().clone())
        .get_post(tonic::Request::new(v1::GetPostRequest {
            post_id: id.into(),
        }))
        .await
        .map(|r| r.into_inner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agents_post_publishes_after_a_person_approves_it() {
    let (s, _dir) = start_fresh(|_| {}, None).await;
    create_repo(&s, "news", v1::Visibility::Public).await;
    create_repo(&s, "secret", v1::Visibility::Public).await;
    let (_, writer) = agent(&s, "post-writer", NEWS).await;
    let (follower_id, follower) = agent(&s, "post-follower", NEWS).await;
    let dev = Client::new(s.client.channel().clone(), s.client.token(), NEWS);
    let posts = |c: &Client| PostServiceClient::new(c.channel().clone());

    // 1. The agent opens a change with a post and adds an image.
    let cr = open_with_post(
        &writer,
        7001,
        "Cache the build layers",
        "Builds are 3x faster",
    )
    .await;
    let draft = cr.post.clone().expect("the change carries its post");
    assert_eq!(draft.state(), v1::PostState::Draft);
    assert_eq!(
        draft.author.as_ref().unwrap().kind(),
        v1::PrincipalKind::Agent
    );
    let image = png(1200, 630);
    let begun = posts(&writer)
        .begin_post_media_upload(writer.req(v1::BeginPostMediaUploadRequest {
            repo: rref(NEWS),
            change_id: cr.id.clone(),
            kind: v1::PostMediaKind::Image as i32,
            content_type: "image/png".into(),
            size_bytes: image.len() as u64,
        }))
        .await
        .expect("begin upload")
        .into_inner();
    // The upload URL refuses what is not an image, then takes the bytes.
    let (status, _, _) = http(
        "PUT",
        &begun.upload_url,
        &[],
        b"<svg onload=alert(1)>".to_vec(),
    )
    .await;
    assert_eq!(status, 415);
    let ct: Vec<(&str, &str)> = begun
        .upload_headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (status, _, body) = http("PUT", &begun.upload_url, &ct, image.clone()).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let wrong = posts(&writer)
        .finalize_post_media(writer.req(v1::FinalizePostMediaRequest {
            repo: rref(NEWS),
            media_id: begun.media_id.clone(),
            sha256: "00".repeat(32),
            alt_text: "Build times before and after".into(),
            ..Default::default()
        }))
        .await
        .expect_err("wrong digest");
    assert_eq!(reason(wrong), "INTEGRITY");
    let media = posts(&writer)
        .finalize_post_media(writer.req(v1::FinalizePostMediaRequest {
            repo: rref(NEWS),
            media_id: begun.media_id.clone(),
            sha256: hex_sha(&image),
            alt_text: "Build times before and after".into(),
            ..Default::default()
        }))
        .await
        .expect("finalize")
        .into_inner();
    assert_eq!((media.width, media.height), (1200, 630));
    let set = posts(&writer)
        .set_change_post(writer.req(v1::SetChangePostRequest {
            repo: rref(NEWS),
            change_id: cr.id.clone(),
            text: "Builds are 3x faster #ci".into(),
            media_ids: vec![media.id.clone()],
            expected_version: 0,
            ..Default::default()
        }))
        .await
        .expect("set post")
        .into_inner();
    assert_eq!(set.media.len(), 1);

    // The follower (another agent) follows the repo and notes where its feed stands.
    UserServiceClient::new(follower.channel().clone())
        .follow_repo(follower.req(v1::FollowRepoRequest { repo: rref(NEWS) }))
        .await
        .expect("an agent follows a repo");
    let feed = |after: String| {
        let f = follower.clone();
        async move {
            PostServiceClient::new(f.channel().clone())
                .list_following_feed(f.req(v1::ListFollowingFeedRequest {
                    page: Some(v1::PageRequest {
                        page_size: 10,
                        page_token: String::new(),
                    }),
                    after,
                }))
                .await
                .expect("feed")
                .into_inner()
        }
    };
    let before = feed(String::new()).await;
    assert!(before.items.is_empty());

    // 2. It lands; an agent's post waits for a person.
    dev.land_change(&cr.id).await.expect("land");
    let pending = get_post(&dev, &set.id).await.expect("get post");
    assert_eq!(pending.state(), v1::PostState::PendingApproval);
    assert!(pending.viewer.as_ref().unwrap().can_approve);
    assert!(feed(String::new()).await.items.is_empty());
    // No agent may approve, its author least of all.
    let r = posts(&writer)
        .approve_post(writer.req(v1::ApprovePostRequest {
            repo: rref(NEWS),
            post_id: set.id.clone(),
        }))
        .await
        .expect_err("agents cannot approve");
    assert_eq!(reason(r), "PERMISSION_DENIED");

    // 3. The dev user approves; it publishes.
    let published = posts(&dev)
        .approve_post(dev.req(v1::ApprovePostRequest {
            repo: rref(NEWS),
            post_id: set.id.clone(),
        }))
        .await
        .expect("approve")
        .into_inner();
    assert_eq!(published.state(), v1::PostState::Published);
    assert_eq!(published.approved_by.as_ref().unwrap().id, s.user());

    // 4. In the follower's feed; polling from the earlier cursor finds it, then nothing.
    let f = feed(before.newest_cursor.clone()).await;
    assert_eq!(f.items.len(), 1);
    assert_eq!(f.items[0].post.as_ref().unwrap().id, set.id);
    let again = feed(f.newest_cursor.clone()).await;
    assert!(again.items.is_empty());
    assert_eq!(again.newest_cursor, f.newest_cursor);
    let followers = UserServiceClient::new(dev.channel().clone())
        .list_repo_followers(dev.req(v1::ListRepoFollowersRequest {
            repo: rref(NEWS),
            page: None,
        }))
        .await
        .expect("followers")
        .into_inner();
    assert!(followers.followers.iter().any(|p| p.id == follower_id));

    // 5. Signed out: the public post and its image; nothing of a private repo.
    let public = get_post_signed_out(&dev, &set.id)
        .await
        .expect("anonymous GetPost");
    assert_eq!(public.text, "Builds are 3x faster #ci");
    let url = public.media[0].url.clone();
    let (status, headers, body) = http("GET", &url, &[], Vec::new()).await;
    assert_eq!(status, 200);
    assert_eq!(body, image);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .starts_with("sandbox")
    );
    let (status, headers, body) = http("GET", &url, &[("range", "bytes=0-7")], Vec::new()).await;
    assert_eq!((status, body.as_slice()), (206, &image[..8]));
    assert_eq!(
        headers["content-range"].to_str().unwrap(),
        format!("bytes 0-7/{}", image.len())
    );
    // A tampered URL is refused.
    let (status, _, _) = http(
        "GET",
        &url.replace("signature=", "signature=0"),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 403);
    let list = PostServiceClient::new(dev.channel().clone())
        .list_repo_posts(tonic::Request::new(v1::ListRepoPostsRequest {
            repo: rref(NEWS),
            page: None,
        }))
        .await
        .expect("anonymous ListRepoPosts")
        .into_inner();
    assert_eq!(list.posts.len(), 1);

    // 6. Withdrawn: gone from the feed and the repo's posts, its image no longer served.
    let dev_url = get_post(&dev, &set.id).await.expect("dev reads").media[0]
        .url
        .clone();
    let gone = posts(&dev)
        .withdraw_post(dev.req(v1::WithdrawPostRequest {
            repo: rref(NEWS),
            post_id: set.id.clone(),
            reason: "numbers were wrong".into(),
        }))
        .await
        .expect("withdraw")
        .into_inner();
    assert_eq!(gone.state(), v1::PostState::Withdrawn);
    assert!(gone.text.is_empty() && gone.media.is_empty());
    assert!(feed(String::new()).await.items.is_empty());
    assert_eq!(http("GET", &url, &[], Vec::new()).await.0, 404);
    assert_eq!(http("GET", &dev_url, &[], Vec::new()).await.0, 404);
    let list = PostServiceClient::new(dev.channel().clone())
        .list_repo_posts(dev.req(v1::ListRepoPostsRequest {
            repo: rref(NEWS),
            page: None,
        }))
        .await
        .expect("ListRepoPosts")
        .into_inner();
    assert!(list.posts.is_empty());

    // 7. A person's post on a public repo publishes on land; signed out, its image is served
    // until the repo turns private.
    let secret = Client::new(s.client.channel().clone(), s.client.token(), SECRET);
    let scr = open_with_post(&secret, 7002, "Rotate keys", "Keys rotated").await;
    let spost = scr.post.clone().expect("post");
    let simg = png(64, 64);
    let b = posts(&secret)
        .begin_post_media_upload(secret.req(v1::BeginPostMediaUploadRequest {
            repo: rref(SECRET),
            change_id: scr.id.clone(),
            kind: v1::PostMediaKind::Image as i32,
            content_type: "image/png".into(),
            size_bytes: simg.len() as u64,
        }))
        .await
        .expect("begin")
        .into_inner();
    assert_eq!(http("PUT", &b.upload_url, &[], simg.clone()).await.0, 200);
    let sm = posts(&secret)
        .finalize_post_media(secret.req(v1::FinalizePostMediaRequest {
            repo: rref(SECRET),
            media_id: b.media_id.clone(),
            sha256: hex_sha(&simg),
            alt_text: "A key".into(),
            ..Default::default()
        }))
        .await
        .expect("finalize")
        .into_inner();
    posts(&secret)
        .set_change_post(secret.req(v1::SetChangePostRequest {
            repo: rref(SECRET),
            change_id: scr.id.clone(),
            text: "Keys rotated".into(),
            media_ids: vec![sm.id],
            expected_version: 0,
            ..Default::default()
        }))
        .await
        .expect("set");
    secret.land_change(&scr.id).await.expect("land");
    let open = get_post_signed_out(&secret, &spost.id)
        .await
        .expect("public while public");
    assert_eq!(open.state(), v1::PostState::Published);
    let surl = open.media[0].url.clone();
    assert_eq!(http("GET", &surl, &[], Vec::new()).await.0, 200);
    RepoServiceClient::new(secret.channel().clone())
        .update_repo(secret.req(v1::UpdateRepoRequest {
            repo: rref(SECRET),
            visibility: v1::Visibility::Private as i32,
            ..Default::default()
        }))
        .await
        .expect("make private");
    let r = get_post_signed_out(&secret, &spost.id)
        .await
        .expect_err("private");
    assert_eq!(reason(r), "NOT_FOUND");
    // Access decisions are cached for at most `forge.access_cache_ms` (1 s by default).
    let mut refused = false;
    for _ in 0..50 {
        if http("GET", &surl, &[], Vec::new()).await.0 == 404 {
            refused = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(refused, "media served after the repo turned private");
    // Its readers still get it.
    let mine = get_post(&secret, &spost.id).await.expect("owner reads");
    assert_eq!(
        http("GET", &mine.media[0].url, &[], Vec::new()).await.0,
        200
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_post_of_its_own_reaches_followers_with_its_cards() {
    let (s, _dir) = start_fresh(|_| {}, None).await;
    create_repo(&s, "news", v1::Visibility::Public).await;
    let (_, writer) = agent(&s, "own-post-writer", NEWS).await;
    let (_, follower) = agent(&s, "own-post-follower", NEWS).await;
    let dev = Client::new(s.client.channel().clone(), s.client.token(), NEWS);
    let posts = |c: &Client| PostServiceClient::new(c.channel().clone());
    UserServiceClient::new(follower.channel().clone())
        .follow_repo(follower.req(v1::FollowRepoRequest { repo: rref(NEWS) }))
        .await
        .expect("an agent follows a repo");
    // Something to attach: an open change request of the repo.
    let cr = open_with_post(&writer, 7101, "Trim the images", "Images are smaller").await;
    let create = |attachments: Vec<v1::PostAttachment>| v1::CreatePostRequest {
        repo: rref(NEWS),
        text: "What we shipped this week".into(),
        body_markdown: "## Images\r\n\r\nHalf the bytes.\n".into(),
        article: Some(v1::PostArticle {
            title: "A week of small wins".into(),
            body_markdown: "# Monday\n\nWe trimmed the images.\n".into(),
        }),
        attachments,
        ..Default::default()
    };
    // Something that is not in the repo cannot be attached, and nothing is written.
    let r = posts(&dev)
        .create_post(dev.req(create(vec![v1::PostAttachment {
            kind: v1::PostAttachmentKind::Issue as i32,
            number: 999_999,
            ..Default::default()
        }])))
        .await
        .expect_err("an issue the repo does not have");
    assert_eq!(reason(r), "INVALID_ARGUMENT");
    // The dev user writes the post: no change request carries it, it is published at once.
    let post = posts(&dev)
        .create_post(dev.req(create(vec![v1::PostAttachment {
            kind: v1::PostAttachmentKind::ChangeRequest as i32,
            number: cr.number,
            ..Default::default()
        }])))
        .await
        .expect("create post")
        .into_inner();
    assert_eq!(post.state(), v1::PostState::Published);
    assert!(post.change_id.is_empty());
    assert_eq!(post.author.as_ref().unwrap().id, s.user());
    assert_eq!(post.body_markdown, "## Images\n\nHalf the bytes.\n");
    assert_eq!(post.article.as_ref().unwrap().title, "A week of small wins");
    assert_eq!(post.attachments.len(), 1);
    let card = &post.attachments[0];
    assert_eq!(card.kind(), v1::PostAttachmentKind::ChangeRequest);
    assert_eq!(
        (
            card.id.as_str(),
            card.number,
            card.title.as_str(),
            card.state.as_str()
        ),
        (cr.id.as_str(), cr.number, "Trim the images", "open")
    );
    // It is in the follower's feed, and a signed-out reader gets it on the public repo.
    let feed = PostServiceClient::new(follower.channel().clone())
        .list_following_feed(follower.req(v1::ListFollowingFeedRequest {
            page: Some(v1::PageRequest {
                page_size: 10,
                page_token: String::new(),
            }),
            after: String::new(),
        }))
        .await
        .expect("feed")
        .into_inner();
    assert_eq!(feed.items.len(), 1);
    assert_eq!(feed.items[0].post.as_ref().unwrap().id, post.id);
    let public = get_post_signed_out(&dev, &post.id)
        .await
        .expect("anonymous GetPost");
    assert_eq!(public.article, post.article);
    assert_eq!(public.attachments, post.attachments);
    // The change request's own post is unchanged by all this: a draft with its change attached.
    let draft = cr.post.clone().expect("the change carries its post");
    assert_eq!(draft.state(), v1::PostState::Draft);
    assert_eq!(draft.attachments.len(), 1);
    assert_eq!(draft.attachments[0].id, cr.id);
}
