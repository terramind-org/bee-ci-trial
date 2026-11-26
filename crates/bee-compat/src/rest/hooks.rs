//! Repo and org webhooks: CRUD, config, ping, deliveries and redelivery. Hooks created here
//! use the GitHub payload format; GitHub event names map to the forge's event kinds through
//! the module registry.

use std::collections::BTreeSet;
use std::sync::Arc;

use bee_proto::v1::event_service_server::EventService as _;
use bee_proto::v1::{
    CreateWebhookRequest, DeleteWebhookRequest, GetWebhookDeliveryRequest,
    ListWebhookDeliveriesRequest, ListWebhooksRequest, PingWebhookRequest, RedeliverWebhookRequest,
    RepoRef, UpdateWebhookRequest, Webhook, WebhookDelivery, WebhookFormat,
};
use serde_json::{Value, json};

use super::{b, repo_of, rid, s, strs};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::time::iso;

/// Where a hook lives.
#[derive(Clone)]
struct Owner {
    repo: Option<RepoRef>,
    org: String,
    path: String,
    kind: &'static str,
}

async fn repo_owner(cx: &Ctx, p: &Params) -> ApiResult<Owner> {
    let repo = repo_of(cx, p).await?;
    Ok(Owner {
        repo: rid(&repo.id),
        org: String::new(),
        path: format!("/repos/{}/hooks", repo.full_name),
        kind: "Repository",
    })
}

fn org_owner(p: &Params) -> Owner {
    Owner {
        repo: None,
        org: p.get("org").to_string(),
        path: format!("/orgs/{}/hooks", p.get("org")),
        kind: "Organization",
    }
}

/// Forge event kinds for GitHub event names.
fn kinds_for(cx: &Ctx, events: &[String]) -> Vec<String> {
    if events.is_empty() || events.iter().any(|e| e == "*") {
        return vec!["*".into()];
    }
    let mut out = BTreeSet::new();
    for m in cx.hub.forge.registry.modules() {
        for k in m.event_kinds {
            if let Some((name, _)) = k.github
                && events.iter().any(|e| e == name)
            {
                out.insert(k.name.to_string());
            }
        }
    }
    for e in events {
        if let Some(rest) = e.strip_prefix("bee_") {
            out.insert(rest.replacen('_', ".", 1));
        }
        if cx.hub.forge.registry.event_kind(e).is_some() {
            out.insert(e.clone());
        }
    }
    out.into_iter().collect()
}

/// GitHub event names for forge kinds.
fn events_for(cx: &Ctx, kinds: &[String]) -> Vec<String> {
    if kinds.iter().any(|k| k == "*") {
        return vec!["*".into()];
    }
    let mut out = BTreeSet::new();
    for k in kinds {
        match cx.hub.forge.registry.event_kind(k).and_then(|e| e.github) {
            Some((name, _)) => out.insert(name.to_string()),
            None => out.insert(format!("bee_{}", k.replace('.', "_"))),
        };
    }
    out.into_iter().collect()
}

fn delivery_status(d: &WebhookDelivery) -> &'static str {
    if d.success { "OK" } else { "failed" }
}

fn hook_json(cx: &Ctx, o: &Owner, h: &Webhook) -> Value {
    let id = cx.hub.ids.remember("hook", &h.id, &o.path);
    let url = cx.api(&format!("{}/{id}", o.path));
    let last = match &h.last_delivery {
        Some(d) => {
            json!({"code": d.status_code, "status": if d.success { "active" } else { "failed" }, "message": d.error})
        }
        None => json!({"code": Value::Null, "status": "unused", "message": Value::Null}),
    };
    json!({
        "type": o.kind,
        "id": id,
        "name": "web",
        "active": h.active,
        "events": events_for(cx, &h.kinds),
        "config": {
            "content_type": "json",
            "insecure_ssl": if h.insecure_ssl { "1" } else { "0" },
            "url": h.url,
        },
        "updated_at": iso(&h.created_at),
        "created_at": iso(&h.created_at),
        "url": url,
        "test_url": format!("{url}/tests"),
        "ping_url": format!("{url}/pings"),
        "deliveries_url": format!("{url}/deliveries"),
        "last_response": last,
    })
}

async fn hooks_of(cx: &Ctx, o: &Owner) -> ApiResult<Vec<Webhook>> {
    let svc = cx.hub.svc.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWebhooksRequest {
            repo: o.repo.clone(),
            org: o.org.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_webhooks(req).await?.into_inner();
            Ok((r.webhooks, r.page))
        }
    })
    .await
}

async fn hook_by_num(cx: &Ctx, o: &Owner, n: u64) -> ApiResult<Webhook> {
    hooks_of(cx, o)
        .await?
        .into_iter()
        .find(|h| crate::ids::num("hook", &h.id) == n)
        .ok_or_else(ApiError::not_found)
}

async fn do_list(cx: &Ctx, o: Owner) -> ApiResult<Reply> {
    let items = hooks_of(cx, &o)
        .await?
        .iter()
        .map(|h| hook_json(cx, &o, h))
        .collect();
    Ok(cx.paginate(items))
}

async fn do_create(cx: &Ctx, o: Owner) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    if let Some(name) = s(&body, "name")
        && name != "web"
    {
        return Err(ApiError::validation(
            "Hook",
            "name",
            "invalid",
            "only `web` hooks are supported",
        ));
    }
    let config = body.get("config").cloned().unwrap_or(Value::Null);
    let url = s(&config, "url")
        .ok_or_else(|| ApiError::validation("Hook", "url", "custom", "url cannot be blank"))?
        .to_string();
    if s(&config, "content_type") == Some("form") {
        return Err(ApiError::validation(
            "Hook",
            "content_type",
            "invalid",
            "only `json` payloads are supported",
        ));
    }
    let events = strs(&body, "events").unwrap_or_else(|| vec!["push".into()]);
    let insecure = match config.get("insecure_ssl") {
        Some(Value::String(v)) => v == "1",
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    };
    let h = cx
        .hub
        .svc
        .create_webhook(cx.req(CreateWebhookRequest {
            repo: o.repo.clone(),
            url,
            kinds: kinds_for(cx, &events),
            secret: s(&config, "secret").unwrap_or("").to_string(),
            org: o.org.clone(),
            format: WebhookFormat::Github as i32,
            inactive: b(&body, "active") == Some(false),
            insecure_ssl: insecure,
        }))
        .await?
        .into_inner();
    Ok(Reply::created(hook_json(cx, &o, &h)))
}

async fn do_update(cx: &Ctx, o: Owner, n: u64, config_only: bool) -> ApiResult<Reply> {
    let h = hook_by_num(cx, &o, n).await?;
    let body = cx.body_json()?;
    let config = if config_only {
        body.clone()
    } else {
        body.get("config").cloned().unwrap_or(Value::Null)
    };
    let mut events: Option<Vec<String>> = None;
    if !config_only {
        if let Some(e) = strs(&body, "events") {
            events = Some(e);
        }
        let mut cur = events_for(cx, &h.kinds);
        if let Some(add) = strs(&body, "add_events") {
            cur.extend(add);
            events = Some(cur.clone());
        }
        if let Some(rm) = strs(&body, "remove_events") {
            let base = events.clone().unwrap_or(cur);
            events = Some(base.into_iter().filter(|e| !rm.contains(e)).collect());
        }
    }
    let insecure = match config.get("insecure_ssl") {
        Some(Value::String(v)) => Some(v == "1"),
        Some(Value::Number(x)) => Some(x.as_u64() == Some(1)),
        _ => None,
    };
    let h = cx
        .hub
        .svc
        .update_webhook(
            cx.req(UpdateWebhookRequest {
                repo: o.repo.clone(),
                id: h.id.clone(),
                org: o.org.clone(),
                url: s(&config, "url").map(str::to_string),
                kinds: events
                    .as_ref()
                    .map(|e| kinds_for(cx, e))
                    .unwrap_or_default(),
                set_kinds: events.is_some(),
                secret: s(&config, "secret").map(str::to_string),
                format: None,
                active: if config_only {
                    None
                } else {
                    b(&body, "active")
                },
                insecure_ssl: insecure,
            }),
        )
        .await?
        .into_inner();
    let v = hook_json(cx, &o, &h);
    Ok(Reply::json(if config_only {
        v["config"].clone()
    } else {
        v
    }))
}

async fn do_delete(cx: &Ctx, o: Owner, n: u64) -> ApiResult<Reply> {
    let h = hook_by_num(cx, &o, n).await?;
    cx.hub
        .svc
        .delete_webhook(cx.req(DeleteWebhookRequest {
            repo: o.repo.clone(),
            id: h.id,
            org: o.org.clone(),
        }))
        .await?;
    Ok(Reply::no_content())
}

async fn do_ping(cx: &Ctx, o: Owner, n: u64) -> ApiResult<Reply> {
    let h = hook_by_num(cx, &o, n).await?;
    cx.hub
        .svc
        .ping_webhook(cx.req(PingWebhookRequest {
            repo: o.repo.clone(),
            id: h.id,
            org: o.org.clone(),
        }))
        .await?;
    Ok(Reply::no_content())
}

fn delivery_json(cx: &Ctx, d: &WebhookDelivery, repo_num: Option<u64>) -> Value {
    let (event, action) = match d.event_kind.as_str() {
        "ping" => ("ping".to_string(), Value::Null),
        k => match cx.hub.forge.registry.event_kind(k).and_then(|e| e.github) {
            Some((n, a)) => (
                n.to_string(),
                if a.is_empty() { Value::Null } else { json!(a) },
            ),
            None => (format!("bee_{}", k.replace('.', "_")), Value::Null),
        },
    };
    json!({
        "id": cx.hub.ids.remember("delivery", &d.id, &d.webhook_id),
        "guid": d.id,
        "delivered_at": iso(&d.delivered_at),
        "redelivery": d.redelivery,
        "duration": d.duration_ms as f64 / 1000.0,
        "status": delivery_status(d),
        "status_code": d.status_code,
        "event": event,
        "action": action,
        "installation_id": Value::Null,
        "repository_id": repo_num,
        "throttled_at": Value::Null,
    })
}

async fn do_deliveries(cx: &Ctx, o: Owner, n: u64) -> ApiResult<Reply> {
    let h = hook_by_num(cx, &o, n).await?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWebhookDeliveriesRequest {
            repo: o.repo.clone(),
            webhook_id: h.id.clone(),
            org: o.org.clone(),
            success: None,
            page: Some(page),
        });
        async move {
            let r = svc.list_webhook_deliveries(req).await?.into_inner();
            Ok((r.deliveries, r.page))
        }
    })
    .await?;
    let repo_num = (!h.repo_id.is_empty()).then(|| crate::ids::num("repo", &h.repo_id));
    let items = list
        .iter()
        .map(|d| delivery_json(cx, d, repo_num))
        .collect();
    Ok(cx.paginate(items))
}

async fn delivery_id(cx: &Ctx, o: &Owner, h: &Webhook, n: u64) -> ApiResult<String> {
    if let Some(ix) = cx.hub.ids.get("delivery", n) {
        return Ok(ix.id);
    }
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWebhookDeliveriesRequest {
            repo: o.repo.clone(),
            webhook_id: h.id.clone(),
            org: o.org.clone(),
            success: None,
            page: Some(page),
        });
        async move {
            let r = svc.list_webhook_deliveries(req).await?.into_inner();
            Ok((r.deliveries, r.page))
        }
    })
    .await?;
    list.into_iter()
        .find(|d| crate::ids::num("delivery", &d.id) == n)
        .map(|d| d.id)
        .ok_or_else(ApiError::not_found)
}

/// `GET /repos/{owner}/{repo}/hooks`.
pub(crate) async fn list(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    do_list(&cx, o).await
}

/// `POST /repos/{owner}/{repo}/hooks`.
pub(crate) async fn create(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    do_create(&cx, o).await
}

/// `GET /repos/{owner}/{repo}/hooks/{id}`.
pub(crate) async fn get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    let h = hook_by_num(&cx, &o, p.num("id")?).await?;
    Ok(Reply::json(hook_json(&cx, &o, &h)))
}

/// `PATCH /repos/{owner}/{repo}/hooks/{id}`.
pub(crate) async fn update(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    do_update(&cx, o, p.num("id")?, false).await
}

/// `DELETE /repos/{owner}/{repo}/hooks/{id}`.
pub(crate) async fn delete(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    do_delete(&cx, o, p.num("id")?).await
}

/// `POST /repos/{owner}/{repo}/hooks/{id}/pings` (and `/tests`).
pub(crate) async fn ping(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    do_ping(&cx, o, p.num("id")?).await
}

/// `GET /repos/{owner}/{repo}/hooks/{id}/config`.
pub(crate) async fn config(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    let h = hook_by_num(&cx, &o, p.num("id")?).await?;
    Ok(Reply::json(hook_json(&cx, &o, &h)["config"].clone()))
}

/// `PATCH /repos/{owner}/{repo}/hooks/{id}/config`.
pub(crate) async fn update_config(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    do_update(&cx, o, p.num("id")?, true).await
}

/// `GET /repos/{owner}/{repo}/hooks/{id}/deliveries`.
pub(crate) async fn deliveries(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    do_deliveries(&cx, o, p.num("id")?).await
}

fn parse_headers(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(json!({}))
}

fn parse_body(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// `GET /repos/{owner}/{repo}/hooks/{id}/deliveries/{delivery}`.
pub(crate) async fn delivery(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    let h = hook_by_num(&cx, &o, p.num("id")?).await?;
    let did = delivery_id(&cx, &o, &h, p.num("delivery")?).await?;
    let d = cx
        .hub
        .svc
        .get_webhook_delivery(cx.req(GetWebhookDeliveryRequest {
            repo: o.repo.clone(),
            webhook_id: h.id.clone(),
            delivery_id: did,
            org: o.org.clone(),
        }))
        .await?
        .into_inner();
    let mut v = delivery_json(
        &cx,
        &d,
        (!h.repo_id.is_empty()).then(|| crate::ids::num("repo", &h.repo_id)),
    );
    v["url"] = json!(h.url);
    v["request"] = json!({"headers": parse_headers(&d.request_headers_json), "payload": parse_body(&d.request_body)});
    v["response"] =
        json!({"headers": parse_headers(&d.response_headers_json), "payload": d.response_body});
    Ok(Reply::json(v))
}

/// `POST /repos/{owner}/{repo}/hooks/{id}/deliveries/{delivery}/attempts`.
pub(crate) async fn redeliver(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = repo_owner(&cx, &p).await?;
    let h = hook_by_num(&cx, &o, p.num("id")?).await?;
    let did = delivery_id(&cx, &o, &h, p.num("delivery")?).await?;
    cx.hub
        .svc
        .redeliver_webhook(cx.req(RedeliverWebhookRequest {
            repo: o.repo.clone(),
            webhook_id: h.id.clone(),
            delivery_id: did,
            org: o.org.clone(),
        }))
        .await?;
    Ok(Reply::accepted(json!({})))
}

/// `GET /orgs/{org}/hooks`.
pub(crate) async fn org_list(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    do_list(&cx, org_owner(&p)).await
}

/// `POST /orgs/{org}/hooks`.
pub(crate) async fn org_create(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    do_create(&cx, org_owner(&p)).await
}

/// `GET /orgs/{org}/hooks/{id}`.
pub(crate) async fn org_get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = org_owner(&p);
    let h = hook_by_num(&cx, &o, p.num("id")?).await?;
    Ok(Reply::json(hook_json(&cx, &o, &h)))
}

/// `PATCH /orgs/{org}/hooks/{id}`.
pub(crate) async fn org_update(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    do_update(&cx, org_owner(&p), p.num("id")?, false).await
}

/// `DELETE /orgs/{org}/hooks/{id}`.
pub(crate) async fn org_delete(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    do_delete(&cx, org_owner(&p), p.num("id")?).await
}

/// `POST /orgs/{org}/hooks/{id}/pings`.
pub(crate) async fn org_ping(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    do_ping(&cx, org_owner(&p), p.num("id")?).await
}

/// `GET /orgs/{org}/hooks/{id}/deliveries`.
pub(crate) async fn org_deliveries(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    do_deliveries(&cx, org_owner(&p), p.num("id")?).await
}
