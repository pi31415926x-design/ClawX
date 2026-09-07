use anyhow::Result;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use socket2::{SockRef, TcpKeepalive};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::{JoinError, JoinSet};

#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::TerminateJobObject;

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
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const RESPONSE_QUEUE_SIZE: usize = 128;
const MAX_JOBS: usize = 64;

/// The full set of tool names this binary can ever implement. A node's
/// --tools flag (registration mode only) must be a subset of this list --
/// it declares which of these the node is willing to expose, it can never
/// grant a tool that doesn't exist.
const KNOWN_TOOLS: &[&str] = &["bash_exec", "bash_exec_async", "bash_job_status"];

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug)]
struct JobStore {
    jobs: Mutex<HashMap<String, JobStatus>>,
    next_id: AtomicU64,
}

#[derive(Debug, Clone)]
struct JobStatus {
    state: &'static str,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl JobStore {
    fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn create(&self) -> Option<(String, JobStatus)> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs.len() >= MAX_JOBS {
            return None;
        }
        let id = format!("job-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let status = JobStatus {
            state: "running",
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
        };
        jobs.insert(id.clone(), status.clone());
        Some((id, status))
    }

    fn update(&self, id: &str, status: JobStatus) {
        if let Some(job) = self
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            *job = status;
        }
    }

    fn get(&self, id: &str) -> Option<JobStatus> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
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
    let args: Vec<String> = std::env::args().collect();
    let mut node_id: Option<String> = None;
    let mut dispatcher: Option<String> = None;
    let mut token: Option<String> = None;
    let mut tools_arg: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--node-id" => {
                i += 1;
                node_id = args.get(i).cloned();
            }
            "--dispatcher" => {
                i += 1;
                dispatcher = args.get(i).cloned();
            }
            "--token" => {
                i += 1;
                token = args.get(i).cloned();
            }
            "--tools" => {
                i += 1;
                tools_arg = args.get(i).cloned();
            }
            other => {
                eprintln!("mcp-shell-server: ignoring unknown argument '{other}'");
            }
        }
        i += 1;
    }

    match (node_id, dispatcher) {
        (Some(node_id), Some(dispatcher)) => {
            let token = token
                .or_else(|| std::env::var("MCP_SHELL_TOKEN").ok())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "registration mode requires a token: pass --token or set MCP_SHELL_TOKEN"
                    )
                })?;
            let tools = parse_tools(tools_arg.as_deref())?;
            run_registered(&dispatcher, &node_id, &tools, &token).await
        }
        (None, None) => run_stdio().await,
        _ => Err(anyhow::anyhow!(
            "--node-id and --dispatcher must be given together to enable registration mode"
        )),
    }
}

/// Parses a comma-separated --tools value into a validated list. Rejecting
/// unknown names here (rather than letting the dispatcher discover it
/// later) means a typo fails fast, locally, before ever touching the
/// network -- the same "fail before you commit" instinct as validating a
/// config file before starting a service. Absent flag or empty string
/// means "expose nothing": the node registers and shows up as online, but
/// isn't a valid target for any tool call.
fn parse_tools(raw: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|name| {
            if KNOWN_TOOLS.contains(&name) {
                Ok(name.to_string())
            } else {
                Err(anyhow::anyhow!(
                    "unknown tool '{name}' in --tools (known tools: {})",
                    KNOWN_TOOLS.join(", ")
                ))
            }
        })
        .collect()
}

/// Local stdio mode: reads JSON-RPC requests from stdin, writes responses
/// to stdout. This is the original transport, unchanged, and remains the
/// default so a plain `mcp-proxy`-spawned child process keeps working
/// exactly as before.
async fn run_stdio() -> Result<()> {
    let reader = BufReader::new(stdin());
    let writer = stdout();
    serve(reader, writer).await
}

/// Registration mode: dial out to `dispatcher`, identify as `node_id`
/// exposing `tools`, and once accepted, serve requests over that TCP
/// connection instead of stdio. Reconnects with a capped backoff whenever
/// the connection drops, so a dispatcher restart or a network blip
/// doesn't require restarting this process by hand.
async fn run_registered(
    dispatcher: &str,
    node_id: &str,
    tools: &[String],
    token: &str,
) -> Result<()> {
    const BACKOFF_STEPS_SECS: [u64; 4] = [1, 2, 5, 10];
    let mut backoff_idx = 0usize;

    loop {
        match register_and_serve(dispatcher, node_id, tools, token).await {
            Ok(()) => {
                eprintln!("mcp-shell-server: dispatcher connection closed, reconnecting");
            }
            Err(e) => {
                eprintln!("mcp-shell-server: registration failed: {e}");
            }
        }

        let delay = BACKOFF_STEPS_SECS[backoff_idx.min(BACKOFF_STEPS_SECS.len() - 1)];
        backoff_idx += 1;
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

/// One connection attempt: connect, send the registration frame (protocol
/// v2, see docs/PROTOCOL.md), wait for the dispatcher's response, and on
/// success hand off to `serve`. Returns `Ok(())` if the connection was
/// accepted and later closed cleanly by the peer (EOF), or `Err` for
/// anything that went wrong before or during the handshake.
async fn register_and_serve(
    dispatcher: &str,
    node_id: &str,
    tools: &[String],
    token: &str,
) -> Result<()> {
    let stream = TcpStream::connect(dispatcher).await?;
    if let Err(e) = enable_tcp_keepalive(&stream) {
        eprintln!("mcp-shell-server: failed to enable TCP keepalive: {e}");
    }
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let register_frame = json!({
        "type": "register",
        "version": 2,
        "node_id": node_id,
        "tools": tools,
        "token": token,
    });
    let mut line = serde_json::to_string(&register_frame)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;

    let mut response_line = String::new();
    let bytes_read = reader.read_line(&mut response_line).await?;
    if bytes_read == 0 {
        anyhow::bail!("dispatcher closed the connection before responding to registration");
    }

    let response: Value = serde_json::from_str(response_line.trim())?;
    match response.get("type").and_then(Value::as_str) {
        Some("registered") => {
            eprintln!(
                "mcp-shell-server: registered as '{node_id}' (tools: {}) with dispatcher {dispatcher}",
                tools.join(", ")
            );
        }
        Some("error") => {
            let reason = response
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            anyhow::bail!("dispatcher rejected registration: {reason}");
        }
        _ => anyhow::bail!("unexpected registration response: {response}"),
    }

    serve(reader, write_half).await
}

/// Sets a moderately aggressive OS-level TCP keepalive on the registration
/// socket. This is deliberately NOT an application-level heartbeat frame --
/// docs/PROTOCOL.md still has none, and this doesn't touch the wire
/// protocol at all. It's a kernel feature that makes the "silence = still
/// alive" assumption in `serve`'s read loop actually hold when a NAT/router
/// somewhere in the path drops the connection's state without ever
/// forwarding a FIN/RST to either side: without a keepalive probe, that
/// kind of half-dead connection can sit forever, because the local socket
/// never sees an error and nothing here ever notices the peer is gone.
/// (Observed in practice testing registration across a DDNS/NAT path: a
/// `tools/call` on a stale connection hung indefinitely even though
/// `list_nodes` still reported the node online.)
fn enable_tcp_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(20))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// Core JSON-RPC loop shared by both transports: reads newline-delimited
/// requests from `reader`, dispatches each onto its own task (bounded by
/// `MAX_IN_FLIGHT`), and writes newline-delimited responses to `writer`
/// via a single dedicated writer task so concurrent handlers can never
/// interleave their writes on the wire.
async fn serve<R, W>(reader: R, writer: W) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = reader.lines();

    // A single dedicated writer task owns the output side and is the only
    // thing that ever writes to it. Requests are handled concurrently
    // below, so without this, two tasks finishing at the same moment
    // could interleave their writes and corrupt the JSON-RPC stream on
    // the wire.
    let (tx, mut rx) = mpsc::channel::<String>(RESPONSE_QUEUE_SIZE);
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(mut out) = rx.recv().await {
            out.push('\n');
            if let Err(e) = writer.write_all(out.as_bytes()).await {
                eprintln!("mcp-shell-server: failed to write response: {e}");
                break;
            }
            if let Err(e) = writer.flush().await {
                eprintln!("mcp-shell-server: failed to flush output: {e}");
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
    let jobs = Arc::new(JobStore::new());
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
        let jobs_for_task = jobs.clone();
        in_flight.spawn(async move {
            let permit = match semaphore.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let response = dispatch(req_id, method, params, jobs_for_task).await;
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

    // Input closed. Drain every still-running request instead of dropping
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

async fn dispatch(
    req_id: Value,
    method: String,
    params: Option<Value>,
    jobs: Arc<JobStore>,
) -> JsonRpcResponse {
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
                    },
                    {
                        "name": "bash_exec_async",
                        "description": "Start a bash command in the background and return immediately with a job_id",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"command": {"type": "string"}},
                            "required": ["command"]
                        }
                    },
                    {
                        "name": "bash_job_status",
                        "description": "Get the status and output of a background bash job",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"job_id": {"type": "string"}},
                            "required": ["job_id"]
                        }
                    }
                    ,{
                        "name": "read_image",
                        "description": "Read a local image file and return it as MCP image content. Supports JPEG, PNG and WebP.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Absolute or relative local path to an image file"
                                }
                            },
                            "required": ["path"]
                        }
                    }
                ]
            })),
            error: None,
        },
        "tools/call" => handle_tool_call(req_id, params, jobs).await,
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

/// Builds the platform shell invocation for a user-supplied command string.
///
/// Unix: `setsid bash -c <command>` -- `setsid` puts the whole command tree
/// in a fresh process group so it can be signalled as a unit on timeout.
///
/// Windows: `powershell.exe -Command <command>`. There is no setsid
/// equivalent; process-tree cleanup on timeout is instead handled with a
/// Job Object (see `attach_job`/`terminate_tree` below). The server process
/// itself is expected to already be running elevated (as a service under
/// LocalSystem/an admin account, or a scheduled task set to "run with
/// highest privileges") -- child processes simply inherit that token, there
/// is no per-command elevation step.
fn build_shell_command(command: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("setsid");
        c.arg("bash").arg("-c").arg(command);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("powershell.exe");
        c.arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(command);
        c
    }
}

/// Windows-only: assigns a freshly spawned child to a new Job Object so
/// that terminating the job also terminates any processes the command
/// spawns, not just the top-level `powershell.exe`. Returns `None` (and
/// logs) on failure -- callers still fall back to `child.kill()`, which at
/// least kills the top-level process.
#[cfg(windows)]
fn attach_job(child: &tokio::process::Child) -> Option<win32job::Job> {
    let job = match win32job::Job::create() {
        Ok(job) => job,
        Err(e) => {
            eprintln!("mcp-shell-server: failed to create job object: {e}");
            return None;
        }
    };
    let Some(proc_handle) = child.raw_handle() else {
        eprintln!("mcp-shell-server: child has no process handle");
        return None;
    };
    if let Err(e) = job.assign_process(proc_handle as isize) {
        eprintln!("mcp-shell-server: failed to assign process to job object: {e}");
        return None;
    }
    Some(job)
}

/// Best-effort termination of a timed-out command's whole process tree.
/// Unix uses the process group created by `setsid` + `pkill -TERM -s <pid>`;
/// Windows uses the Job Object attached at spawn time, if any.
async fn terminate_tree(
    child: &mut tokio::process::Child,
    #[cfg(windows)] job: &Option<win32job::Job>,
) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let _ = Command::new("pkill")
                .arg("-TERM")
                .arg("-s")
                .arg(pid.to_string())
                .output()
                .await;
        }
    }
    #[cfg(windows)]
    {
        if let Some(job) = job {
            unsafe {
                let _ = TerminateJobObject(job.handle() as _, 1);
            }
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn execute_command(command: &str) -> std::result::Result<(i32, String, String), String> {
    let mut child = build_shell_command(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Execution failed: {e}"))?;

    #[cfg(windows)]
    let job = attach_job(&child);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout was not piped".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "stderr was not piped".to_string())?;
    let stdout_task = tokio::spawn(read_limited(stdout, MAX_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(read_limited(stderr, MAX_OUTPUT_BYTES));
    let status = match tokio::time::timeout(COMMAND_TIMEOUT, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("Execution failed: {e}")),
        Err(_) => {
            terminate_tree(
                &mut child,
                #[cfg(windows)]
                &job,
            )
            .await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(format!(
                "Execution timed out after {}s",
                COMMAND_TIMEOUT.as_secs()
            ));
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))?;
    let stderr = stderr_task
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))?;
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

async fn handle_read_image(id: Value, args: &Value) -> JsonRpcResponse {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(path) if !path.is_empty() => path,
        _ => return error_response(id, -32602, "Missing path argument"),
    };

    let path = std::path::Path::new(path_str);
    let metadata = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) => return error_response(id, -32000, format!("Cannot stat image: {e}")),
    };

    if !metadata.is_file() {
        return error_response(id, -32602, "Path is not a file");
    }

    let size = metadata.len() as usize;
    if size > MAX_IMAGE_BYTES {
        return error_response(
            id,
            -32602,
            format!("Image is too large: {size} bytes (max {MAX_IMAGE_BYTES})"),
        );
    }

    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => {
            return error_response(
                id,
                -32602,
                "Unsupported image type; use jpg, jpeg, png or webp",
            )
        }
    };

    let data = match tokio::fs::read(path).await {
        Ok(data) => data,
        Err(e) => return error_response(id, -32000, format!("Cannot read image: {e}")),
    };

    let encoded = base64::engine::general_purpose::STANDARD.encode(data);

    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        result: Some(json!({
            "content": [{
                "type": "image",
                "data": encoded,
                "mimeType": mime
            }],
            "isError": false
        })),
        error: None,
    }
}

async fn handle_tool_call(
    id: Value,
    params: Option<Value>,
    jobs: Arc<JobStore>,
) -> JsonRpcResponse {
    let params = params.unwrap_or_default();
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_default();
    if name == "read_image" {
        return handle_read_image(id, &args).await;
    }

    if name == "bash_job_status" {
        let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v,
            _ => return error_response(id, -32602, "Missing job_id argument"),
        };
        return match jobs.get(job_id) {
            Some(job) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: Some(id),
                error: None,
                result: Some(
                    json!({"content":[{"type":"text","text":format!("state={}\nexit_code={:?}\nSTDOUT:\n{}\nSTDERR:\n{}", job.state, job.exit_code, job.stdout, job.stderr)}], "isError": false}),
                ),
            },
            None => error_response(id, -32602, "Unknown job_id"),
        };
    }

    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(cmd) if !cmd.is_empty() => cmd.to_owned(),
        _ => return error_response(id, -32602, "Missing command argument"),
    };

    if name == "bash_exec_async" {
        let (job_id, _) = match jobs.create() {
            Some(v) => v,
            None => return error_response(id, -32000, "Too many background jobs"),
        };
        let jobs_clone = jobs.clone();
        let job_id_clone = job_id.clone();
        tokio::spawn(async move {
            let result = execute_command(&command).await;
            let status = match result {
                Ok((exit_code, stdout, stderr)) => JobStatus {
                    state: "completed",
                    exit_code: Some(exit_code),
                    stdout,
                    stderr,
                },
                Err(message) => JobStatus {
                    state: "failed",
                    exit_code: None,
                    stdout: String::new(),
                    stderr: message,
                },
            };
            jobs_clone.update(&job_id_clone, status);
        });
        return JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            error: None,
            result: Some(
                json!({"content":[{"type":"text","text":format!("job_id={}\nstate=running", job_id)}], "isError": false}),
            ),
        };
    }

    if name != "bash_exec" {
        return error_response(id, -32602, "Unknown tool");
    }

    // Delegates to the same platform-aware spawn/timeout/kill logic used by
    // the async job path, instead of duplicating it here per-platform.
    match execute_command(&command).await {
        Ok((exit_code, stdout_data, stderr_data)) => {
            let combined = format!(
                "Exit Code: {}\nSTDOUT:\n{}\nSTDERR:\n{}",
                exit_code, stdout_data, stderr_data
            );
            // Avoid duplicating potentially large stdout/stderr in structuredContent.
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: Some(id),
                result: Some(json!({
                    "content": [{"type": "text", "text": combined}],
                    "isError": exit_code != 0
                })),
                error: None,
            }
        }
        Err(message) => error_response(id, -32000, message),
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
