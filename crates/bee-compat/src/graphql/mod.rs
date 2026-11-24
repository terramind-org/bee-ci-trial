//! `POST /api/graphql` (and `/graphql` at the root): a hand-written GraphQL executor over a
//! schema subset of GitHub's API.
//!
//! The executor is small and in-crate rather than `async-graphql`: the gateway needs query
//! and mutation execution with variables, fragments, aliases, directives and introspection
//! over one dynamic object model, which is ~1k lines here, while `async-graphql` would add
//! some thirty crates that are not in the lockfile and a derive-based schema that would have
//! to duplicate every GitHub type by hand anyway.

mod exec;
mod parse;
mod resolve;
mod schema;

use std::sync::Arc;

use axum::extract::Request;
use axum::response::Response;
use serde_json::{Map, Value, json};

use crate::error::ApiError;
use crate::http::{Reply, error_reply, finish, rate_error, take_rate};
use crate::hub::Compat;

use self::exec::{Exec, Node};

/// Serves one GraphQL request.
pub(crate) async fn handle(hub: Arc<Compat>, prefix: &str, req: Request) -> Response {
    let cx = match crate::http::context(hub, prefix, req).await {
        Ok(cx) => cx,
        Err(reply) => return finish(None, reply, None),
    };
    if cx.caller.is_none() {
        let e = ApiError::unauthenticated()
            .with_message("This endpoint requires you to be authenticated.");
        return finish(Some(&cx), error_reply(&e), None);
    }
    let state = match take_rate(&cx, "graphql") {
        Ok(s) => s,
        Err(s) => {
            return finish(
                Some(&cx),
                error_reply(&rate_error(&cx, &s)),
                Some((s, "graphql")),
            );
        }
    };
    let body: Value = match serde_json::from_slice(&cx.body) {
        Ok(v) => v,
        Err(_) => {
            return finish(
                Some(&cx),
                error_reply(&ApiError::bad_request("Problems parsing JSON")),
                Some((state, "graphql")),
            );
        }
    };
    let query = body.get("query").and_then(Value::as_str).unwrap_or("");
    let vars: Map<String, Value> = body
        .get("variables")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let op_name = body.get("operationName").and_then(Value::as_str);
    let out = run(cx.clone(), query, op_name, vars).await;
    finish(Some(&cx), Reply::json(out), Some((state, "graphql")))
}

/// Executes `query` and returns the response body (`data` and `errors`).
pub(crate) async fn run(
    cx: Arc<crate::http::Ctx>,
    query: &str,
    op_name: Option<&str>,
    vars: Map<String, Value>,
) -> Value {
    let doc = match parse::parse_document(query) {
        Ok(d) => d,
        Err(e) => {
            return json!({"errors": [{"message": format!("Parse error: {}", e.message), "locations": [{"line": 1, "column": e.offset + 1}]}]});
        }
    };
    let (ex, op) = match Exec::new(cx, &doc, op_name, vars) {
        Ok(x) => x,
        Err(m) => return json!({"errors": [{"message": m}]}),
    };
    let root = if op.mutation {
        Node::Mutation
    } else {
        Node::Query
    };
    let data = ex.object(&root, &op.sel, Vec::new()).await;
    let errors = ex.take_errors();
    if errors.is_empty() {
        json!({"data": data})
    } else {
        json!({"data": data, "errors": errors})
    }
}
