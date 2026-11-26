//! The stdio transport (`bee mcp`).
//!
//! One JSON-RPC message (or batch) per line on stdin; responses and notifications one per line
//! on stdout, which carries nothing else. `tools/call` requests run concurrently and can be
//! cancelled with `notifications/cancelled`; every other request is answered in order, so an
//! `initialize` followed at once by `tools/list` behaves as expected. At end of input the
//! server finishes in-flight calls, then returns.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::protocol::{Incoming, PARSE_ERROR, error_response};
use crate::server::{CallContext, ConfigError, McpConfig, McpServer, Session};

/// Serves MCP on this process's stdin and stdout until stdin closes.
pub async fn run_stdio(config: McpConfig) -> std::io::Result<()> {
    let server = McpServer::new(config).map_err(config_io_error)?;
    serve_stdio(&server, tokio::io::stdin(), tokio::io::stdout()).await
}

fn config_io_error(error: ConfigError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
}

/// Serves MCP over any byte streams (pipes, sockets, in-memory duplexes) until `reader` ends.
pub async fn serve_stdio<R, W>(server: &McpServer, reader: R, writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = rx.recv().await {
            let mut line = serde_json::to_vec(&message).unwrap_or_default();
            line.push(b'\n');
            writer.write_all(&line).await?;
            writer.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    });

    let session = Arc::new(server.new_session());
    let context = CallContext::default();
    let mut calls = JoinSet::new();
    let mut lines = BufReader::new(reader).lines();
    loop {
        tokio::select! {
            Some(_) = calls.join_next(), if !calls.is_empty() => continue,
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(e) => {
                        let _ = tx.send(error_response(
                            Value::Null,
                            PARSE_ERROR,
                            &format!("parse error: {e}"),
                            None,
                        ));
                        continue;
                    }
                };
                dispatch(server, &session, &context, value, &tx, &mut calls).await;
            }
        }
    }
    while calls.join_next().await.is_some() {}
    drop(tx);
    match writer_task.await {
        Ok(result) => result,
        Err(e) => Err(std::io::Error::other(e)),
    }
}

async fn dispatch(
    server: &McpServer,
    session: &Arc<Session>,
    context: &CallContext,
    value: Value,
    tx: &mpsc::UnboundedSender<Value>,
    calls: &mut JoinSet<()>,
) {
    match value {
        Value::Array(batch) if batch.is_empty() => {
            let _ = tx.send(error_response(
                Value::Null,
                crate::protocol::INVALID_REQUEST,
                "empty batch",
                None,
            ));
        }
        Value::Array(batch) => {
            // A batch is answered as one array once every member is done.
            let server = server.clone();
            let session = Arc::clone(session);
            let context = context.clone();
            let tx = tx.clone();
            calls.spawn(async move {
                let mut responses = Vec::new();
                for item in batch {
                    let handled = server
                        .handle(&session, Incoming::classify(item), &context)
                        .await;
                    for n in handled.notifications {
                        let _ = tx.send(n);
                    }
                    responses.extend(handled.response);
                }
                if !responses.is_empty() {
                    let _ = tx.send(Value::Array(responses));
                }
            });
        }
        single => match Incoming::classify(single) {
            Incoming::Request { id, method, params } if method == "tools/call" => {
                let server = server.clone();
                let task_session = Arc::clone(session);
                let context = context.clone();
                let tx = tx.clone();
                let task_id = id.clone();
                // The task waits until it is registered, so it can never untrack itself
                // before being tracked.
                let (tracked_tx, tracked_rx) = tokio::sync::oneshot::channel::<()>();
                let handle = calls.spawn(async move {
                    let _ = tracked_rx.await;
                    let handled = server
                        .handle(
                            &task_session,
                            Incoming::Request {
                                id: task_id.clone(),
                                method,
                                params,
                            },
                            &context,
                        )
                        .await;
                    task_session.untrack(&task_id);
                    for n in handled.notifications {
                        let _ = tx.send(n);
                    }
                    if let Some(response) = handled.response {
                        let _ = tx.send(response);
                    }
                });
                session.track(&id, handle);
                let _ = tracked_tx.send(());
            }
            message => {
                let handled = server.handle(session, message, context).await;
                for n in handled.notifications {
                    let _ = tx.send(n);
                }
                if let Some(response) = handled.response {
                    let _ = tx.send(response);
                }
            }
        },
    }
}
