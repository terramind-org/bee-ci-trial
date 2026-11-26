//! JSON Schemas (draft 2020-12) of the ergonomic commands' `data`. They are embedded in the
//! generated files under `schemas/v1/cli/`.

use serde_json::{Value, json};

fn string() -> Value {
    json!({"type": "string"})
}

fn nullable_string() -> Value {
    json!({"type": ["string", "null"]})
}

fn uint() -> Value {
    json!({"type": "integer", "minimum": 0})
}

fn object(required: &[&str], properties: Value) -> Value {
    json!({
        "type": "object",
        "required": required,
        "additionalProperties": false,
        "properties": properties,
    })
}

fn array(items: Value) -> Value {
    json!({"type": "array", "items": items})
}

/// One change of the stack.
pub fn change_view() -> Value {
    object(
        &[
            "index",
            "changeId",
            "changeset",
            "parent",
            "subject",
            "head",
        ],
        json!({
            "index": {"type": "integer", "minimum": 1, "description": "Position from 1 at the bottom."},
            "changeId": {"type": "string", "pattern": "^k_[0-9a-f]{32}$"},
            "changeset": {"type": "string", "pattern": "^c1_[0-9a-f]{64}$"},
            "parent": {"type": "string", "pattern": "^c1_[0-9a-f]{64}$"},
            "subject": string(),
            "head": {"type": "boolean", "description": "The working copy is based on this change."}
        }),
    )
}

fn conflict_view() -> Value {
    object(
        &["path", "kind", "markers", "resolved"],
        json!({
            "path": string(),
            "kind": {"enum": ["content", "add_add", "modify_delete", "symlink", "binary", "redacted", "mode", "file_directory"]},
            "markers": {"type": "boolean", "description": "The file holds diff3 conflict markers."},
            "resolved": {"type": "boolean"}
        }),
    )
}

fn rebase_view() -> Value {
    json!({
        "oneOf": [
            {"type": "null"},
            object(
                &["kind", "current", "currentSubject", "done", "pending"],
                json!({
                    "kind": {"enum": ["sync", "restack"]},
                    "current": nullable_string(),
                    "currentSubject": nullable_string(),
                    "done": uint(),
                    "pending": uint()
                }),
            )
        ]
    })
}

fn hidden_view() -> Value {
    object(
        &["path", "reason"],
        json!({"path": string(), "reason": string()}),
    )
}

fn land_view() -> Value {
    json!({
        "oneOf": [
            {"type": "null"},
            object(
                &["ref", "target", "version", "attempts", "eventSeq", "landed"],
                json!({
                    "ref": string(),
                    "target": string(),
                    "version": uint(),
                    "attempts": uint(),
                    "eventSeq": uint(),
                    "landed": array(object(
                        &["changeId", "source", "result", "skipped"],
                        json!({
                            "changeId": string(),
                            "source": string(),
                            "result": string(),
                            "skipped": {"type": "boolean"}
                        }),
                    ))
                }),
            )
        ]
    })
}

/// `bee status`.
pub fn status() -> Value {
    object(
        &[
            "root",
            "repo",
            "targetRef",
            "head",
            "onto",
            "ontoVersion",
            "stack",
            "changes",
            "conflicts",
            "rebase",
            "sparse",
            "hidden",
            "skipped",
            "clean",
            "recovered",
        ],
        json!({
            "root": string(),
            "repo": string(),
            "targetRef": string(),
            "head": string(),
            "onto": string(),
            "ontoVersion": uint(),
            "stack": array(change_view()),
            "changes": array(object(
                &["path", "status", "modeChanged"],
                json!({
                    "path": string(),
                    "status": {"enum": ["added", "modified", "deleted"]},
                    "modeChanged": {"type": "boolean"}
                }),
            )),
            "conflicts": array(conflict_view()),
            "rebase": rebase_view(),
            "sparse": array(string()),
            "hidden": array(hidden_view()),
            "skipped": array(string()),
            "clean": {"type": "boolean"},
            "recovered": {"type": ["string", "null"], "description": "Operation finished by crash recovery."}
        }),
    )
}

/// Every mutating working-copy command.
pub fn op_report() -> Value {
    object(
        &[
            "op",
            "seq",
            "summary",
            "head",
            "onto",
            "stack",
            "changeset",
            "changeId",
            "written",
            "removed",
            "conflicts",
            "rebase",
            "dropped",
            "hidden",
            "land",
            "undid",
            "redacted",
            "redactedDirty",
        ],
        json!({
            "op": string(),
            "seq": {"type": "integer", "minimum": 0, "description": "Operation-log sequence number; 0 when nothing changed."},
            "summary": string(),
            "head": string(),
            "onto": string(),
            "stack": array(change_view()),
            "changeset": nullable_string(),
            "changeId": nullable_string(),
            "written": uint(),
            "removed": uint(),
            "conflicts": array(conflict_view()),
            "rebase": rebase_view(),
            "dropped": array(string()),
            "hidden": array(hidden_view()),
            "land": land_view(),
            "undid": nullable_string(),
            "redacted": {"type": "array", "items": string(), "description": "Newly redacted files replaced with the placeholder (SPEC 13)."},
            "redactedDirty": {"type": "array", "items": string(), "description": "Newly redacted files left alone because they hold local changes."}
        }),
    )
}

/// `bee stack list`.
pub fn stack_list() -> Value {
    object(
        &["head", "onto", "targetRef", "stack"],
        json!({
            "head": string(),
            "onto": string(),
            "targetRef": string(),
            "stack": array(change_view())
        }),
    )
}

/// `bee sparse list`.
pub fn sparse_list() -> Value {
    object(&["sparse"], json!({"sparse": array(string())}))
}

/// `bee oplog`.
pub fn oplog() -> Value {
    object(
        &["ops"],
        json!({
            "ops": array(object(
                &["seq", "op", "summary", "undoable", "undoes", "undone"],
                json!({
                    "seq": uint(),
                    "op": string(),
                    "summary": string(),
                    "undoable": {"type": "boolean"},
                    "undoes": {"type": ["integer", "null"]},
                    "undone": {"type": "boolean"}
                }),
            ))
        }),
    )
}

/// `bee diff`.
pub fn diff() -> Value {
    object(
        &["source", "files"],
        json!({
            "source": {"enum": ["working-copy", "server"]},
            "files": array(object(
                &["path", "status", "oldMode", "newMode", "binary", "additions", "deletions", "unified"],
                json!({
                    "path": string(),
                    "status": {"enum": ["added", "modified", "deleted", "renamed", "copied", "mode-changed"]},
                    "oldMode": nullable_string(),
                    "newMode": nullable_string(),
                    "binary": {"type": "boolean"},
                    "additions": uint(),
                    "deletions": uint(),
                    "unified": {"type": "string", "description": "Unified diff text; empty for binary files."}
                }),
            ))
        }),
    )
}

/// `bee cat`.
pub fn cat() -> Value {
    object(
        &["path", "rev", "mode", "blob", "size", "encoding", "content"],
        json!({
            "path": string(),
            "rev": string(),
            "mode": string(),
            "blob": string(),
            "size": uint(),
            "encoding": {"enum": ["utf8", "base64"]},
            "content": string()
        }),
    )
}

fn nullable_uint() -> Value {
    json!({"type": ["integer", "null"], "minimum": 0})
}

fn skipped_paths() -> Value {
    array(object(
        &["path", "reason"],
        json!({"path": string(), "reason": string()}),
    ))
}

fn timings() -> Value {
    object(
        &["listMs", "fetchMs", "materializeMs", "totalMs"],
        json!({
            "listMs": {"type": "number", "minimum": 0},
            "fetchMs": {"type": "number", "minimum": 0},
            "materializeMs": {"type": "number", "minimum": 0},
            "totalMs": {"type": "number", "minimum": 0}
        }),
    )
}

fn snapshot_fields() -> (Vec<&'static str>, Value) {
    (
        vec![
            "changeset",
            "files",
            "symlinks",
            "bytes",
            "linked",
            "cloned",
            "copied",
            "fetched",
            "skipped",
            "timings",
        ],
        json!({
            "changeset": {"type": "string", "pattern": "^c1_[0-9a-f]{64}$"},
            "files": uint(),
            "symlinks": uint(),
            "bytes": uint(),
            "linked": uint(),
            "cloned": uint(),
            "copied": uint(),
            "fetched": {"type": "integer", "minimum": 0, "description": "Objects that had to come from the server."},
            "skipped": skipped_paths(),
            "timings": timings()
        }),
    )
}

/// `bee daemon prefetch`: what the daemon did.
pub fn snapshot_report() -> Value {
    let (required, properties) = snapshot_fields();
    object(&required, properties)
}

/// `bee snapshot`: the daemon's report plus the working copy it became.
pub fn snapshot() -> Value {
    let (mut required, mut properties) = snapshot_fields();
    required.extend(["repo", "revision", "dir", "head", "matchesHead"]);
    if let Some(map) = properties.as_object_mut() {
        map.insert("repo".into(), string());
        map.insert("revision".into(), string());
        map.insert("dir".into(), string());
        map.insert(
            "head".into(),
            json!({"type": "string", "pattern": "^c1_[0-9a-f]{64}$"}),
        );
        map.insert(
            "matchesHead".into(),
            json!({"type": "boolean", "description": "The snapshot is the checked-out change; false means `bee status` shows a difference."}),
        );
    }
    object(&required, properties)
}

/// `bee daemon start`.
pub fn daemon_start() -> Value {
    object(
        &["running", "started", "socket", "pid", "upstream", "log"],
        json!({
            "running": {"type": "boolean"},
            "started": {"type": "boolean", "description": "False when a daemon already answered."},
            "socket": string(),
            "pid": nullable_uint(),
            "upstream": nullable_string(),
            "log": nullable_string()
        }),
    )
}

/// `bee daemon stop`.
pub fn daemon_stop() -> Value {
    object(
        &["stopped", "socket", "pid"],
        json!({
            "stopped": {"type": "boolean", "description": "False when no daemon was running."},
            "socket": string(),
            "pid": nullable_uint()
        }),
    )
}

/// `bee daemon status`.
pub fn daemon_status() -> Value {
    object(
        &["running", "socket", "pid", "upstream", "tier"],
        json!({
            "running": {"type": "boolean"},
            "socket": string(),
            "pid": nullable_uint(),
            "upstream": nullable_string(),
            "tier": nullable_string()
        }),
    )
}

/// `bee daemon stats`: the daemon's counters (more may be added).
pub fn daemon_stats() -> Value {
    let counters = [
        "hits",
        "misses",
        "failClosed",
        "bytesServed",
        "upstreamRequests",
        "upstreamObjects",
        "decisionChecks",
        "signedUrlFetches",
        "signedUrlFallbacks",
        "integrityFailures",
        "refHits",
        "refMisses",
        "forwarded",
    ];
    let mut required = vec!["tier", "hitRate", "store"];
    required.extend(counters);
    let mut properties = serde_json::Map::new();
    properties.insert("tier".into(), json!({"enum": ["daemon", "regional"]}));
    properties.insert(
        "hitRate".into(),
        json!({"type": "number", "minimum": 0, "maximum": 1}),
    );
    for c in counters {
        properties.insert(c.into(), uint());
    }
    properties.insert(
        "store".into(),
        json!({
            "type": "object",
            "required": ["objects", "bytes", "capacity", "evictions", "corruptDropped", "pinned"],
            "properties": {
                "objects": uint(),
                "bytes": uint(),
                "capacity": uint(),
                "evictions": uint(),
                "corruptDropped": uint(),
                "pinned": uint()
            }
        }),
    );
    json!({
        "type": "object",
        "required": required,
        "properties": properties,
    })
}

/// `bee daemon pin`.
pub fn daemon_pin() -> Value {
    object(
        &["name", "changeset", "objects", "skipped"],
        json!({
            "name": string(),
            "changeset": {"type": "string", "pattern": "^c1_[0-9a-f]{64}$"},
            "objects": uint(),
            "skipped": skipped_paths()
        }),
    )
}

/// `bee daemon unpin`.
pub fn daemon_unpin() -> Value {
    object(&["removed"], json!({"removed": {"type": "boolean"}}))
}
