use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::{JoinError, JoinSet};

/// Commands get killed (see `kill_on_drop` below) and turned into a timeout
/// error if they run longer than this, so a single hung `bash_exec` call
/// can't pin a task (and its process) forever.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Upper bound on requests being handled at once. This is a safety valve,
/// not a real limit on throughput: it just stops an unbounded burst of
/// requests from spawning unboundedly many tasks/child processes before
/// any of them finish.
const MAX_IN_FLIGHT: usize = 16;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const RESPONSE_QUEUE_SIZE: usize = 128;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut reader = BufReader::new(stdin()).lines();

    // A single dedicated writer task owns stdout and is the only thing
    // that ever writes to it. Requests are handled concurrently below, so
    // without this, two tasks finishing at the same moment could interleave
    // their writes and corrupt the JSON-RPC stream on the wire.
    let (tx, mut rx) = mpsc::channel::<String>(RESPONSE_QUEUE_SIZE);
    let writer_task = tokio::spawn(async move {
        let mut writer = stdout();
        while let Some(mut out) = rx.recv().await {
            out.push('\n');
            if let Err(e) = writer.write_all(out.as_bytes()).await {
                eprintln!("mcp-shell-server: failed to write response: {e}");
                break;
            }
            if let Err(e) = writer.flush().await {
                eprintln!("mcp-shell-server: failed to flush stdout: {e}");
                break;
            }
        }
    });

    // Each request is dispatched onto its own task so a slow or hanging
    // `bash_exec` call can never block reading, or responding to, any
    // other in-flight request. Responses are matched back to their caller
    // purely via the JSON-RPC `id`, so completing out of request order is
    // fine per spec.
    //
    // Tasks are tracked in a JoinSet (rather than fire-and-forget) so that
    // every task's outcome is actually observed: a panic inside a handler
    // is caught by tokio and reported through the JoinHandle instead of
    // silently vanishing or, worse, taking down the whole process -- the
    // Rust analogue of letting a child task's exception escape an
    // unattended TaskGroup/ExceptionGroup.
    let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let mut in_flight: JoinSet<()> = JoinSet::new();

    while let Ok(Some(line)) = reader.next_line().await {
        if line.trim().is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let response = error_response(Value::Null, -32700, format!("Parse error: {e}"));
                if let Ok(out) = serde_json::to_string(&response) {
                    if tx.send(out).await.is_err() {
                        break;
                    }
                }
                continue;
            }
        };

        // Notifications (no id) get no response.
        let JsonRpcRequest {
            id, method, params, ..
        } = req;
        let Some(req_id) = id else { continue };

        let tx = tx.clone();
        let semaphore = semaphore.clone();
        in_flight.spawn(async move {
            let permit = match semaphore.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let response = dispatch(req_id, method, params).await;
            drop(permit);
            match serde_json::to_string(&response) {
                Ok(out) => {
                    let _ = tx.send(out).await;
                }
                Err(e) => eprintln!("mcp-shell-server: failed to serialize response: {e}"),
            }
        });

        while let Some(res) = in_flight.try_join_next() {
            log_join_result(res);
        }
    }

    // Stdin closed. Drain every still-running request instead of dropping
    // their JoinHandles, so a handler that was mid-flight gets to send its
    // response (or have its panic logged) before we exit.
    while let Some(res) = in_flight.join_next().await {
        log_join_result(res);
    }
    drop(tx);
    let _ = writer_task.await;

    Ok(())
}

fn log_join_result(res: std::result::Result<(), JoinError>) {
    if let Err(e) = res {
        if e.is_panic() {
            eprintln!("mcp-shell-server: a request handler panicked: {e}");
        } else {
            eprintln!("mcp-shell-server: a request handler was cancelled: {e}");
        }
    }
}

async fn dispatch(req_id: Value, method: String, params: Option<Value>) -> JsonRpcResponse {
    match method.as_str() {
        "initialize" => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(req_id),
            result: Some(json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "rust-mcp-shell",
                    "version": "0.1.0"
                }
            })),
            error: None,
        },
        "tools/list" => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(req_id),
            result: Some(json!({
                "tools": [
                    {
                        "name": "bash_exec",
                        "description": "Execute a bash command safely on the host",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "command": {
                                    "type": "string",
                                    "description": "The shell command to be executed"
                                }
                            },
                            "required": ["command"]
                        },
                        "outputSchema": {
                            "type": "object",
                            "properties": {
                                "exit_code": {"type": "integer"},
                                "stdout": {"type": "string"},
                                "stderr": {"type": "string"}
                            },
                            "required": ["exit_code", "stdout", "stderr"]
                        }
                    }
                ]
            })),
            error: None,
        },
        "tools/call" => handle_tool_call(req_id, params).await,
        _ => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(req_id),
            result: None,
            error: Some(json!({ "code": -32601, "message": "Method not found" })),
        },
    }
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        result: None,
        error: Some(json!({ "code": code, "message": message.into() })),
    }
}

async fn handle_tool_call(id: Value, params: Option<Value>) -> JsonRpcResponse {
    let params = params.unwrap_or_default();
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_default();
    if name != "bash_exec" {
        return error_response(id, -32602, "Unknown tool");
    }
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(cmd) if !cmd.is_empty() => cmd.to_owned(),
        _ => return error_response(id, -32602, "Missing command argument"),
    };

    let mut child = match Command::new("setsid")
        .arg("bash")
        .arg("-c")
        .arg(&command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return error_response(id, -32000, format!("Execution failed: {e}")),
    };

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_task = tokio::spawn(read_limited(stdout, MAX_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(read_limited(stderr, MAX_OUTPUT_BYTES));

    let status = match tokio::time::timeout(COMMAND_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return error_response(id, -32000, format!("Execution failed: {e}")),
        Err(_) => {
            if let Some(pid) = child.id() {
                let _ = Command::new("pkill")
                    .arg("-TERM")
                    .arg("-s")
                    .arg(pid.to_string())
                    .output()
                    .await;
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return error_response(
                id,
                -32000,
                format!("Execution timed out after {}s", COMMAND_TIMEOUT.as_secs()),
            );
        }
    };

    let stdout_data = match stdout_task.await {
        Ok(Ok(v)) => v,
        _ => String::new(),
    };
    let stderr_data = match stderr_task.await {
        Ok(Ok(v)) => v,
        _ => String::new(),
    };

    let combined = format!(
        "Exit Code: {}\nSTDOUT:\n{}\nSTDERR:\n{}",
        status.code().unwrap_or(-1),
        stdout_data,
        stderr_data
    );
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        result: Some(json!({
            "content": [{"type": "text", "text": combined}],
            "structuredContent": {"exit_code": status.code().unwrap_or(-1), "stdout": stdout_data, "stderr": stderr_data},
            "isError": !status.success()
        })),
        error: None,
    }
}

async fn read_limited<R>(mut reader: R, limit: usize) -> std::io::Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let remaining = limit.saturating_sub(buf.len());
        let take = n.min(remaining);
        buf.extend_from_slice(&chunk[..take]);
        if take < n {
            truncated = true;
        }
        // Continue draining after the limit so the child cannot block on a full pipe.
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        s.push_str("\n[output truncated at 8 MiB]");
    }
    Ok(s)
}
