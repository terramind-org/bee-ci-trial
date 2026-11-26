//! RubyGems: push, compact index, download, yank, legacy specs index.

use bee_forge::packages::hash::{md5_hex, sha256_hex};
use bytes::Bytes;

use super::support::*;

fn spec(name: &str, version: &str) -> String {
    format!(
        "--- !ruby/object:Gem::Specification\nname: {name}\nversion: !ruby/object:Gem::Version\n  version: {version}\nplatform: ruby\nauthors:\n- Ada\nsummary: A widget gem\ndescription: longer\nhomepage: https://beehive.test/acme/app\nlicenses:\n- MIT\ndependencies:\n- !ruby/object:Gem::Dependency\n  name: rake\n  requirement: !ruby/object:Gem::Requirement\n    requirements:\n    - - \">=\"\n      - !ruby/object:Gem::Version\n        version: '12.0'\n  type: :runtime\n  prerelease: false\n- !ruby/object:Gem::Dependency\n  name: rspec\n  requirement: !ruby/object:Gem::Requirement\n    requirements:\n    - - \"~>\"\n      - !ruby/object:Gem::Version\n        version: '3.0'\n  type: :development\n  prerelease: false\nrequired_ruby_version: !ruby/object:Gem::Requirement\n  requirements:\n  - - \">=\"\n    - !ruby/object:Gem::Version\n      version: 2.5.0\nrequired_rubygems_version: !ruby/object:Gem::Requirement\n  requirements:\n  - - \">=\"\n    - !ruby/object:Gem::Version\n      version: '0'\n"
    )
}

fn gem(name: &str, version: &str) -> Vec<u8> {
    let meta = gzip(spec(name, version).as_bytes());
    let data = gzip(&tar(&[("lib/widget.rb", b"puts 1")]));
    tar(&[("metadata.gz", &meta), ("data.tar.gz", &data)])
}

#[tokio::test]
async fn push_info_download_and_yank() {
    let w = world().await;
    let tok = w.tok("owner");
    let g1 = gem("widget", "1.0.0");
    let r = w
        .http(rq(
            "POST",
            "/rubygems/acme/api/v1/gems",
            Some(&tok),
            &[("authorization", &tok)],
            g1.clone(),
        ))
        .await;
    assert_eq!(r.status(), 200, "{}", body_str(&r));
    // Repushing is refused.
    let r = w
        .http(rq(
            "POST",
            "/rubygems/acme/api/v1/gems",
            Some(&tok),
            &[],
            g1.clone(),
        ))
        .await;
    assert_eq!(r.status(), 409);
    let g2 = gem("widget", "1.1.0");
    assert_eq!(
        w.http(rq(
            "POST",
            "/rubygems/acme/api/v1/gems",
            Some(&tok),
            &[],
            g2
        ))
        .await
        .status(),
        200
    );
    // Not a gem.
    assert_eq!(
        w.http(rq(
            "POST",
            "/rubygems/acme/api/v1/gems",
            Some(&tok),
            &[],
            b"junk".to_vec()
        ))
        .await
        .status(),
        400
    );
    // The gemspec's homepage linked the gem to acme/app, so org members can read it.
    let reader = w.tok("reader");
    let info = w
        .http(rq(
            "GET",
            "/rubygems/acme/info/widget",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(info.status(), 200);
    let text = body_str(&info);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "---");
    assert_eq!(lines.len(), 3);
    assert_eq!(
        lines[1],
        format!(
            "1.0.0 rake:>= 12.0|checksum:{},ruby:>= 2.5.0",
            sha256_hex(&g1)
        )
    );
    assert!(hdr(&info, "etag").contains(&md5_hex(text.as_bytes())));
    // The versions file lists the gem with the md5 of its info file.
    let v = w
        .http(rq(
            "GET",
            "/rubygems/acme/versions",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    let vt = body_str(&v);
    assert!(vt.starts_with("created_at: "));
    assert!(
        vt.contains(&format!("widget 1.0.0,1.1.0 {}", md5_hex(text.as_bytes()))),
        "{vt}"
    );
    // Download counts.
    let d = w
        .http(rq(
            "GET",
            "/rubygems/acme/gems/widget-1.0.0.gem",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(d.status(), 200);
    assert_eq!(d.body().as_ref(), g1.as_slice());
    let j = body_json(
        &w.http(rq(
            "GET",
            "/rubygems/acme/api/v1/gems/widget.json",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await,
    );
    assert_eq!(j["name"], "widget");
    assert_eq!(j["downloads"], 1);
    let vs = body_json(
        &w.http(rq(
            "GET",
            "/rubygems/acme/api/v1/versions/widget.json",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await,
    );
    assert_eq!(vs.as_array().unwrap().len(), 2);
    // Yank 1.0.0: listed with a minus, hidden from info, not downloadable; unyank restores.
    let y = w
        .http(rq(
            "DELETE",
            "/rubygems/acme/api/v1/gems/yank",
            Some(&tok),
            &[("content-type", "application/x-www-form-urlencoded")],
            "gem_name=widget&version=1.0.0",
        ))
        .await;
    assert_eq!(y.status(), 200);
    let vt = body_str(
        &w.http(rq(
            "GET",
            "/rubygems/acme/versions",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await,
    );
    assert!(
        vt.contains("widget -1.0.0,1.1.0 ") || vt.contains("widget 1.1.0,-1.0.0 "),
        "{vt}"
    );
    let info = body_str(
        &w.http(rq(
            "GET",
            "/rubygems/acme/info/widget",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await,
    );
    assert_eq!(info.lines().count(), 2);
    assert_eq!(
        w.http(rq(
            "GET",
            "/rubygems/acme/gems/widget-1.0.0.gem",
            Some(&reader),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        404
    );
    let u = w
        .http(rq(
            "PUT",
            "/rubygems/acme/api/v1/gems/unyank?gem_name=widget&version=1.0.0",
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(u.status(), 200);
    assert_eq!(
        w.http(rq(
            "GET",
            "/rubygems/acme/gems/widget-1.0.0.gem",
            Some(&reader),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        200
    );
    // Legacy specs index: a gzip of a Marshal 4.8 array.
    let s = w
        .http(rq(
            "GET",
            "/rubygems/acme/specs.4.8.gz",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(s.status(), 200);
    let raw = bee_forge::packages::archive::gunzip(s.body(), 1 << 20).unwrap();
    assert_eq!(&raw[..3], &[4, 8, b'[']);
    assert!(raw.windows(6).any(|w| w == b"widget"));
    // Auth.
    assert_eq!(
        w.http(rq(
            "GET",
            "/rubygems/acme/info/widget",
            None,
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        401
    );
    assert_eq!(
        w.http(rq(
            "GET",
            "/rubygems/acme/info/widget",
            Some(&w.tok("outsider")),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        404
    );
    assert_eq!(
        w.http(rq(
            "POST",
            "/rubygems/acme/api/v1/gems",
            Some(&w.tok("reader")),
            &[],
            gem("widget", "9.9.9")
        ))
        .await
        .status(),
        403
    );
}
