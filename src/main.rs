use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use socket2::{SockRef, TcpKeepalive};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
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
const MAX_JOBS: usize = 64;

/// The full set of tool names this binary can ever implement. A node's
/// --tools flag (registration mode only) must be a subset of this list --
/// it declares which of these the node is willing to expose, it can never
/// grant a tool that doesn't exist.
const KNOWN_TOOLS: &[&str] = &["bash_exec", "bash_exec_async", "bash_job_status"];

/// Set once at startup from --verbose, read from wherever a task is about
/// to be dispatched. A plain global instead of threading a bool through
/// every function (parse args -> run_stdio/run_registered -> serve ->
/// dispatch) because it's a cross-cutting concern -- "print what you're
/// about to do" -- not part of any of those functions' actual job.
static VERBOSE: AtomicBool = AtomicBool::new(false);

const COLOR_RESET: &str = "\x1b[0m";
const COLOR_CYAN: &str = "\x1b[36m";
const COLOR_YELLOW: &str = "\x1b[33m";
const COLOR_GREEN: &str = "\x1b[32m";

/// stdout/stderr on a stock Windows console (conhost, not Windows
/// Terminal) don't interpret ANSI escapes unless a process explicitly
/// opts in via ENABLE_VIRTUAL_TERMINAL_PROCESSING. Unix terminals need no
/// such thing. This is the one Windows-only startup step; everywhere else
/// colored output is just an eprintln! with escape codes, same on both
/// platforms.
#[cfg(windows)]
mod win_console {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(nStdHandle: i32) -> *mut c_void;
        fn GetConsoleMode(hConsoleHandle: *mut c_void, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: *mut c_void, dwMode: u32) -> i32;
    }

    const STD_OUTPUT_HANDLE: i32 = -11;
    const STD_ERROR_HANDLE: i32 = -12;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    /// Best-effort: if there's no real console attached (piped, or a
    /// service host) the mode calls just fail and we silently move on --
    /// colored output degrades to raw escape codes in the output, same as
    /// it would on a dumb terminal on Unix.
    pub fn enable_ansi_colors() {
        for handle_id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            unsafe {
                let handle = GetStdHandle(handle_id);
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    continue;
                }
                let mut mode: u32 = 0;
                if GetConsoleMode(handle, &mut mode) == 0 {
                    continue;
                }
                let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

/// Prints a received JSON-RPC task to stderr, in color, when --verbose is
/// set. Called from `serve()` so it covers both transports (stdio and
/// registered/TCP) from a single call site.
fn log_task_verbose(id: &Value, method: &str, params: &Option<Value>) {
    if !VERBOSE.load(Ordering::Relaxed) {
        return;
    }
    let params_str = params
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "null".to_string());
    eprintln!(
        "{CYAN}[verbose]{RESET} task received  {GREEN}method{RESET}={YELLOW}{method}{RESET}  id={id}  params={params_str}",
        CYAN = COLOR_CYAN,
        GREEN = COLOR_GREEN,
        YELLOW = COLOR_YELLOW,
        RESET = COLOR_RESET,
    );
}

/// Prints --help and the full option/example reference, then the caller
/// exits. Kept as plain text (no clap/structopt dependency) to match the
/// rest of this file's hand-rolled arg parsing.
fn print_help() {
    println!(
        r#"mcp-shell-server {version} -- runs shell commands on behalf of an MCP client

USAGE:
    mcp-shell-server [OPTIONS]

Two mutually exclusive modes, chosen by whether --node-id/--dispatcher
are given:

  stdio mode (default, no flags required):
    Reads JSON-RPC requests from stdin, writes responses to stdout. This
    is how a local mcp-proxy spawns this binary as a child process.

  registration mode (--node-id and --dispatcher together):
    Dials out to a dispatcher (mcp-proxy) over TCP, registers as a named
    node, and serves requests over that connection instead of stdio.
    Reconnects with backoff on a dropped connection, but gives up after
    5 consecutive failed connection attempts rather than retrying forever.

OPTIONS:
    -h, --help
            Print this help and exit.

    --node-id <ID>
            This node's identifier, as it will appear to dispatcher
            clients (registration mode). Required together with
            --dispatcher.
            Example: --node-id gpu-worker-01

    --dispatcher <HOST:PORT>
            Address of the mcp-proxy dispatcher to register with
            (registration mode). Required together with --node-id.
            Example: --dispatcher seoul.ddns.edux.dev:8383

    --token <TOKEN>
            Registration auth token. Can be given here or via the
            MCP_SHELL_TOKEN environment variable instead (useful so the
            token doesn't show up in `ps`). Required in registration mode.
            Example: --token hi.com9981
            Example: MCP_SHELL_TOKEN=hi.com9981 mcp-shell-server --node-id gpu-worker-01 --dispatcher seoul.ddns.edux.dev:8383

    --user <USER>
            The person this node belongs to (protocol v3). Can be given
            here or via MCP_SHELL_USER instead. Required in registration
            mode.
            Example: --user haogle

    --pwd <PWD>
            Password identifying --user (protocol v3). Can be given here
            or via MCP_SHELL_PWD instead. Required in registration mode.
            Example: --pwd abc.com998

    --tools <NAME,NAME,...>
            Comma-separated subset of the tools this node exposes.
            Unknown names are rejected at startup rather than accepted
            and failing later. Omit or pass "" to register online but
            expose nothing.
            Known tools: {known_tools}
            Example: --tools bash_exec,bash_exec_async,bash_job_status

    --verbose
            Print every received task (method, id, params) to stderr in
            color as it comes in. Works in both stdio and registration
            mode, and on both Linux and Windows consoles.
            Example: mcp-shell-server --verbose

EXAMPLES:
    # Local stdio mode, spawned by mcp-proxy:
    mcp-shell-server

    # Register with a dispatcher, exposing all tools, verbose logging:
    mcp-shell-server --node-id gpu-worker-01 \
        --dispatcher seoul.ddns.edux.dev:8383 \
        --user haogle --pwd abc.com998 --token hi.com9981 \
        --tools bash_exec,bash_exec_async,bash_job_status \
        --verbose
"#,
        version = env!("CARGO_PKG_VERSION"),
        known_tools = KNOWN_TOOLS.join(", "),
    );
}


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

    // Checked before anything else, and outside the normal option loop
    // below, so `--help` works no matter what else is (or isn't) on the
    // command line, including a half-finished/invalid invocation.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    #[cfg(windows)]
    win_console::enable_ansi_colors();

    let mut node_id: Option<String> = None;
    let mut dispatcher: Option<String> = None;
    let mut token: Option<String> = None;
    let mut tools_arg: Option<String> = None;
    let mut user: Option<String> = None;
    let mut pwd: Option<String> = None;
    let mut verbose = false;

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
            "--verbose" => {
                verbose = true;
            }
            "--tools" => {
                i += 1;
                tools_arg = args.get(i).cloned();
            }
            "--user" => {
                i += 1;
                user = args.get(i).cloned();
            }
            "--pwd" => {
                i += 1;
                pwd = args.get(i).cloned();
            }
            other => {
                eprintln!("mcp-shell-server: ignoring unknown argument '{other}'");
            }
        }
        i += 1;
    }

    VERBOSE.store(verbose, Ordering::Relaxed);

    match (node_id, dispatcher) {
        (Some(node_id), Some(dispatcher)) => {
            let token = token
                .or_else(|| std::env::var("MCP_SHELL_TOKEN").ok())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "registration mode requires a token: pass --token or set MCP_SHELL_TOKEN"
                    )
                })?;
            // `user`/`pwd` identify which *person* this node belongs to
            // (protocol v3, see docs/PROTOCOL.md) -- separate from
            // `token`, which only answers "is this connection allowed to
            // register at all". The dispatcher rejects registration
            // outright (`missing_credentials`) if either is empty, so
            // fail the same way locally rather than making a doomed round
            // trip over the network first.
            let user = user
                .or_else(|| std::env::var("MCP_SHELL_USER").ok())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "registration mode requires a user: pass --user or set MCP_SHELL_USER"
                    )
                })?;
            let pwd = pwd
                .or_else(|| std::env::var("MCP_SHELL_PWD").ok())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "registration mode requires a pwd: pass --pwd or set MCP_SHELL_PWD"
                    )
                })?;
            let tools = parse_tools(tools_arg.as_deref())?;
            run_registered(&dispatcher, &node_id, &tools, &token, &user, &pwd).await
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
/// owned by `user` and exposing `tools`, and once accepted, serve requests
/// over that TCP
/// connection instead of stdio. Reconnects with a capped backoff whenever
/// the connection drops, so a dispatcher restart or a network blip
/// doesn't require restarting this process by hand.
async fn run_registered(
    dispatcher: &str,
    node_id: &str,
    tools: &[String],
    token: &str,
    user: &str,
    pwd: &str,
) -> Result<()> {
    const BACKOFF_STEPS_SECS: [u64; 4] = [1, 2, 5, 10];
    // A misconfigured --dispatcher (typo'd host, firewalled port, wrong
    // token) should fail loudly and let the process exit -- not spin
    // forever with backoff, quietly burning a restart-loop slot in
    // whatever supervises this process. A connection that *did* succeed
    // and later dropped is a different, normal situation (network blip,
    // dispatcher restart) and keeps the original unlimited-retry
    // behavior; only *consecutive* failures without ever reaching a
    // working connection count against this limit.
    const MAX_CONSECUTIVE_FAILURES: u32 = 5;
    let mut backoff_idx = 0usize;
    let mut consecutive_failures: u32 = 0;

    loop {
        match register_and_serve(dispatcher, node_id, tools, token, user, pwd).await {
            Ok(()) => {
                eprintln!("mcp-shell-server: dispatcher connection closed, reconnecting");
                consecutive_failures = 0;
                backoff_idx = 0;
            }
            Err(e) => {
                consecutive_failures += 1;
                eprintln!(
                    "mcp-shell-server: registration failed ({consecutive_failures}/{MAX_CONSECUTIVE_FAILURES}): {e}"
                );
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(anyhow::anyhow!(
                        "giving up after {MAX_CONSECUTIVE_FAILURES} consecutive failed attempts to connect to dispatcher {dispatcher}"
                    ));
                }
            }
        }

        let delay = BACKOFF_STEPS_SECS[backoff_idx.min(BACKOFF_STEPS_SECS.len() - 1)];
        backoff_idx += 1;
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

/// Builds the registration frame (protocol v3, see docs/PROTOCOL.md).
/// Pulled out of `register_and_serve` as a pure function so its shape can
/// be unit tested without opening a real TCP connection.
fn build_register_frame(node_id: &str, tools: &[String], token: &str, user: &str, pwd: &str) -> Value {
    json!({
        "type": "register",
        "version": 3,
        "node_id": node_id,
        "tools": tools,
        "token": token,
        "user": user,
        "pwd": pwd,
    })
}

/// One connection attempt: connect, send the registration frame (protocol
/// v3, see docs/PROTOCOL.md), wait for the dispatcher's response, and on
/// success hand off to `serve`. Returns `Ok(())` if the connection was
/// accepted and later closed cleanly by the peer (EOF), or `Err` for
/// anything that went wrong before or during the handshake.
async fn register_and_serve(
    dispatcher: &str,
    node_id: &str,
    tools: &[String],
    token: &str,
    user: &str,
    pwd: &str,
) -> Result<()> {
    let stream = TcpStream::connect(dispatcher).await?;
    if let Err(e) = enable_tcp_keepalive(&stream) {
        eprintln!("mcp-shell-server: failed to enable TCP keepalive: {e}");
    }
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let register_frame = build_register_frame(node_id, tools, token, user, pwd);
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
                "mcp-shell-server: registered as '{node_id}' (user: {user}, tools: {}) with dispatcher {dispatcher}",
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
        .with_interval(Duration::from_secs(10));
    // Windows has no TCP_KEEPCNT-equivalent knob, so socket2 only exposes
    // `with_retries` on Unix-like targets; Windows just uses its OS default
    // retry count on top of the time/interval set above.
    #[cfg(not(windows))]
    let keepalive = keepalive.with_retries(3);
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
        log_task_verbose(id.as_ref().unwrap_or(&Value::Null), &method, &params);
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

/// Builds the child-process command for running a shell one-liner,
/// platform-appropriate: `setsid bash -c <command>` on Unix (so we get a
/// process group we can signal as a whole on timeout, see
/// `kill_process_group_on_timeout` below), `powershell -Command <command>`
/// on Windows (no setsid/bash there; PowerShell is the closest thing to a
/// universally-present shell on a stock Windows box).
fn shell_command(command: &str) -> Command {
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("setsid");
        cmd.arg("bash").arg("-c").arg(command);
        cmd
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("powershell");
        cmd.arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(command);
        cmd
    }
}

/// Best-effort escalation when a command times out: on Unix, `setsid`
/// gave the child its own process group, so send TERM to the whole group
/// (covers subprocesses the command itself spawned, not just the direct
/// child) before the harder `child.kill()`. On Windows there's no such
/// group and no `pkill`, so this is a no-op there -- `child.kill()` alone
/// has to do.
async fn kill_process_group_on_timeout(pid: u32) {
    #[cfg(not(windows))]
    {
        let _ = Command::new("pkill")
            .arg("-TERM")
            .arg("-s")
            .arg(pid.to_string())
            .output()
            .await;
    }
    #[cfg(windows)]
    {
        let _ = pid;
    }
}

async fn execute_command(command: &str) -> std::result::Result<(i32, String, String), String> {
    let mut child = shell_command(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Execution failed: {e}"))?;
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
            if let Some(pid) = child.id() {
                kill_process_group_on_timeout(pid).await;
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
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

async fn handle_tool_call(
    id: Value,
    params: Option<Value>,
    jobs: Arc<JobStore>,
) -> JsonRpcResponse {
    let params = params.unwrap_or_default();
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_default();
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

    let mut child = match shell_command(&command)
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
                kill_process_group_on_timeout(pid).await;
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
    // Avoid duplicating potentially large stdout/stderr in structuredContent.
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        result: Some(json!({
            "content": [{"type": "text", "text": combined}],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tools_accepts_known_names_and_trims_whitespace() {
        let tools = parse_tools(Some(" bash_exec, bash_exec_async ")).unwrap();
        assert_eq!(tools, vec!["bash_exec", "bash_exec_async"]);
    }

    #[test]
    fn parse_tools_absent_or_empty_means_nothing_exposed() {
        assert_eq!(parse_tools(None).unwrap(), Vec::<String>::new());
        assert_eq!(parse_tools(Some("")).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn parse_tools_rejects_unknown_names() {
        assert!(parse_tools(Some("bash_exec,not_a_real_tool")).is_err());
    }

    #[test]
    fn register_frame_is_protocol_v3_with_user_and_pwd() {
        let tools = vec!["bash_exec".to_string()];
        let frame = build_register_frame("gpu-node", &tools, "tok", "alice", "hunter2");
        assert_eq!(frame["type"], "register");
        assert_eq!(frame["version"], 3);
        assert_eq!(frame["node_id"], "gpu-node");
        assert_eq!(frame["tools"], json!(["bash_exec"]));
        assert_eq!(frame["token"], "tok");
        assert_eq!(frame["user"], "alice");
        assert_eq!(frame["pwd"], "hunter2");
    }
}
