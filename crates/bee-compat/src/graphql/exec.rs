//! The executor: operation selection, variables, field collection with fragments and
//! `@include`/`@skip`, validation against the schema, and value completion
//! (GraphQL specification, section 6).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bee_proto::v1::{ChangeRequest, Issue, Org, Principal, PrincipalKind, Release, Repo};
use serde_json::{Map, Value, json};

use super::parse::{Directive, Document, Field, Operation, Selection, Val};
use super::schema::schema;
use crate::error::ApiError;
use crate::http::Ctx;
use crate::ports::GitCommitInfo;

/// A resolved value: a leaf, an object to select from, a list, or null.
#[derive(Clone)]
pub(crate) enum Out {
    Null,
    Leaf(Value),
    Obj(Node),
    List(Vec<Out>),
}

impl Out {
    /// A leaf from anything serializable.
    pub fn leaf(v: impl Into<Value>) -> Out {
        Out::Leaf(v.into())
    }

    /// A string leaf, or null when empty.
    pub fn opt_str(s: &str) -> Out {
        if s.is_empty() {
            Out::Null
        } else {
            Out::leaf(s)
        }
    }

    /// A map object.
    pub fn map(ty: &str, fields: Vec<(&str, Out)>) -> Out {
        Out::Obj(Node::Map(
            ty.to_string(),
            Arc::new(
                fields
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            ),
        ))
    }
}

/// An object the executor selects fields from.
#[derive(Clone)]
pub(crate) enum Node {
    Query,
    Mutation,
    /// A precomputed object.
    Map(String, Arc<BTreeMap<String, Out>>),
    /// A JSON object whose nested objects take their type from the schema.
    Json(String, Arc<Value>),
    Repo(Arc<Repo>),
    Pull(Arc<Repo>, Arc<ChangeRequest>),
    Issue(Arc<Repo>, Arc<Issue>),
    Actor(Arc<Principal>),
    Org(Arc<Org>),
    /// A commit, with the PR target branch when reached through a PR (required checks).
    Commit(Arc<Repo>, Arc<GitCommitInfo>, Option<String>),
    /// The status check rollup of a commit.
    Rollup(Arc<Repo>, String, Option<String>),
    Release(Arc<Repo>, Arc<Release>),
}

impl Node {
    /// The GraphQL type name.
    pub fn type_name(&self) -> String {
        match self {
            Node::Query => "Query".into(),
            Node::Mutation => "Mutation".into(),
            Node::Map(t, _) | Node::Json(t, _) => t.clone(),
            Node::Repo(_) => "Repository".into(),
            Node::Pull(..) => "PullRequest".into(),
            Node::Issue(..) => "Issue".into(),
            Node::Actor(p) => match PrincipalKind::try_from(p.kind).unwrap_or_default() {
                PrincipalKind::Agent | PrincipalKind::Service => "Bot".into(),
                _ => "User".into(),
            },
            Node::Org(_) => "Organization".into(),
            Node::Commit(..) => "Commit".into(),
            Node::Rollup(..) => "StatusCheckRollup".into(),
            Node::Release(..) => "Release".into(),
        }
    }
}

/// A resolver error: GitHub's `type` and message.
#[derive(Clone, Debug)]
pub(crate) struct GqlError {
    pub kind: Option<&'static str>,
    pub message: String,
}

impl GqlError {
    /// An error with a type.
    pub fn new(kind: &'static str, message: impl Into<String>) -> GqlError {
        GqlError {
            kind: Some(kind),
            message: message.into(),
        }
    }
}

impl From<ApiError> for GqlError {
    fn from(e: ApiError) -> GqlError {
        let kind = match e.status {
            404 => "NOT_FOUND",
            403 => "FORBIDDEN",
            401 => "UNAUTHENTICATED",
            409 => "CONFLICT",
            405 | 422 => "UNPROCESSABLE",
            429 => "RATE_LIMITED",
            _ => "INTERNAL",
        };
        let mut message = e.message.clone();
        for item in &e.errors {
            if let Some(m) = item.get("message").and_then(Value::as_str)
                && !m.is_empty()
            {
                message = format!("{message}: {m}");
            }
        }
        GqlError {
            kind: Some(kind),
            message,
        }
    }
}

impl From<tonic::Status> for GqlError {
    fn from(s: tonic::Status) -> GqlError {
        GqlError::from(ApiError::from(s))
    }
}

/// A resolver result.
pub(crate) type GResult = Result<Out, GqlError>;

/// Boxed futures for the recursive executor.
pub(crate) type BoxF<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One execution of one operation.
pub(crate) struct Exec<'a> {
    pub cx: Arc<Ctx>,
    pub doc: &'a Document,
    pub vars: Map<String, Value>,
    pub errors: Mutex<Vec<Value>>,
}

/// A value with variables substituted, as JSON.
pub(crate) fn val_json(v: &Val, vars: &Map<String, Value>) -> Value {
    match v {
        Val::Null => Value::Null,
        Val::Bool(b) => json!(b),
        Val::Int(i) => json!(i),
        Val::Float(f) => json!(f),
        Val::Str(s) | Val::Enum(s) => json!(s),
        Val::List(l) => Value::Array(l.iter().map(|x| val_json(x, vars)).collect()),
        Val::Obj(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), val_json(x, vars)))
                .collect(),
        ),
        Val::Var(n) => vars.get(n).cloned().unwrap_or(Value::Null),
    }
}

impl<'a> Exec<'a> {
    /// Prepares an execution: picks the operation and binds variables with defaults.
    pub fn new(
        cx: Arc<Ctx>,
        doc: &'a Document,
        name: Option<&str>,
        vars: Map<String, Value>,
    ) -> Result<(Exec<'a>, &'a Operation), String> {
        let op = match name.filter(|n| !n.is_empty()) {
            Some(n) => doc
                .ops
                .iter()
                .find(|o| o.name.as_deref() == Some(n))
                .ok_or_else(|| format!("Unknown operation named '{n}'."))?,
            None if doc.ops.len() == 1 => &doc.ops[0],
            None => {
                return Err(
                    "An operation name is required when the document has several operations."
                        .into(),
                );
            }
        };
        let mut bound = vars;
        for v in &op.vars {
            if !bound.contains_key(&v.name)
                && let Some(d) = &v.default
            {
                bound.insert(v.name.clone(), val_json(d, &Map::new()));
            }
        }
        Ok((
            Exec {
                cx,
                doc,
                vars: bound,
                errors: Mutex::new(Vec::new()),
            },
            op,
        ))
    }

    fn included(&self, ds: &[Directive]) -> bool {
        for d in ds {
            let cond = d
                .args
                .iter()
                .find(|(n, _)| n == "if")
                .map(|(_, v)| val_json(v, &self.vars))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            match d.name.as_str() {
                "include" if !cond => return false,
                "skip" if cond => return false,
                _ => {}
            }
        }
        true
    }

    fn collect<'s>(
        &'s self,
        ty: &str,
        sel: &'s [Selection],
        out: &mut Vec<(String, Vec<&'s Field>)>,
        depth: usize,
    ) {
        if depth > 32 {
            return;
        }
        for s in sel {
            match s {
                Selection::Field(f) => {
                    if !self.included(&f.directives) {
                        continue;
                    }
                    match out.iter_mut().find(|(k, _)| k == f.key()) {
                        Some((_, v)) => v.push(f),
                        None => out.push((f.key().to_string(), vec![f])),
                    }
                }
                Selection::Spread { name, directives } => {
                    if !self.included(directives) {
                        continue;
                    }
                    if let Some(fr) = self.doc.fragments.get(name)
                        && schema().matches(ty, &fr.on)
                    {
                        self.collect(ty, &fr.sel, out, depth + 1);
                    }
                }
                Selection::Inline {
                    on,
                    directives,
                    sel,
                } => {
                    if !self.included(directives) {
                        continue;
                    }
                    if on.as_deref().is_none_or(|c| schema().matches(ty, c)) {
                        self.collect(ty, sel, out, depth + 1);
                    }
                }
            }
        }
    }

    fn error(&self, e: GqlError, path: &[Value]) {
        let mut v =
            json!({"message": e.message, "path": path, "locations": [{"line": 1, "column": 1}]});
        if let Some(k) = e.kind {
            v["type"] = json!(k);
        }
        if let Ok(mut errs) = self.errors.lock() {
            errs.push(v);
        }
    }

    /// Selects `sel` from `node`.
    pub fn object<'s>(
        &'s self,
        node: &'s Node,
        sel: &'s [Selection],
        path: Vec<Value>,
    ) -> BoxF<'s, Value> {
        Box::pin(async move {
            let ty = node.type_name();
            let mut groups = Vec::new();
            self.collect(&ty, sel, &mut groups, 0);
            let mut out = Map::new();
            for (key, fields) in groups {
                let f = fields[0];
                let mut fpath = path.clone();
                fpath.push(json!(key));
                if f.name == "__typename" {
                    out.insert(key, json!(ty));
                    continue;
                }
                let known_type = schema().get(&ty).is_some();
                let introspection = f.name.starts_with("__");
                if known_type && !introspection && schema().field(&ty, &f.name).is_none() {
                    self.error(
                        GqlError::new(
                            "UNDEFINED_FIELD",
                            format!("Field '{}' doesn't exist on type '{ty}'", f.name),
                        ),
                        &fpath,
                    );
                    out.insert(key, Value::Null);
                    continue;
                }
                let args: Map<String, Value> = f
                    .args
                    .iter()
                    .map(|(n, v)| (n.clone(), val_json(v, &self.vars)))
                    .collect();
                let resolved = super::resolve::field(self, node, &f.name, &args).await;
                let value = match resolved {
                    Ok(o) => {
                        let merged: Vec<Selection> =
                            fields.iter().flat_map(|x| x.sel.iter().cloned()).collect();
                        self.complete(o, merged, fpath).await
                    }
                    Err(e) => {
                        self.error(e, &fpath);
                        Value::Null
                    }
                };
                out.insert(key, value);
            }
            Value::Object(out)
        })
    }

    fn complete<'s>(&'s self, o: Out, sel: Vec<Selection>, path: Vec<Value>) -> BoxF<'s, Value> {
        Box::pin(async move {
            match o {
                Out::Null => Value::Null,
                Out::Leaf(v) => v,
                Out::Obj(n) => {
                    if sel.is_empty() {
                        return Value::Object(Map::new());
                    }
                    let sel_ref: &[Selection] = &sel;
                    // `sel` lives for this future; the executor borrows it for the call.
                    let n_ref = &n;
                    self.object(n_ref, sel_ref, path).await
                }
                Out::List(items) => {
                    let mut arr = Vec::with_capacity(items.len());
                    for (i, it) in items.into_iter().enumerate() {
                        let mut p = path.clone();
                        p.push(json!(i));
                        arr.push(self.complete(it, sel.clone(), p).await);
                    }
                    Value::Array(arr)
                }
            }
        })
    }

    /// The child node for a JSON field: typed by the schema's field type (or `__typename`).
    pub fn json_child(&self, parent: &str, field: &str, v: &Value) -> Out {
        let ty = schema()
            .field(parent, field)
            .map(|f| f.ty.base().to_string())
            .unwrap_or_default();
        json_out(&ty, v)
    }

    /// The errors so far.
    pub fn take_errors(&self) -> Vec<Value> {
        self.errors.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

/// A JSON value as an [`Out`] of type `ty` (objects become [`Node::Json`]).
pub(crate) fn json_out(ty: &str, v: &Value) -> Out {
    match v {
        Value::Null => Out::Null,
        Value::Array(a) => Out::List(a.iter().map(|x| json_out(ty, x)).collect()),
        Value::Object(m) => {
            let t = m
                .get("__typename")
                .and_then(Value::as_str)
                .unwrap_or(ty)
                .to_string();
            Out::Obj(Node::Json(t, Arc::new(v.clone())))
        }
        other => Out::Leaf(other.clone()),
    }
}
