//! Owned app-server with a stdio control client and an independently attachable TUI.
//! Task input never enters the terminal; no reconnect or request replay is implicit.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig, Message};

const LIMIT: usize = 8 * 1024 * 1024;
const TOKEN_ENV: &str = "CCB_CODEX_TUI_HOST_TOKEN";

pub(crate) fn validate(spec: &Value) -> Result<()> {
    if spec["tui_host_root"].is_null() {
        return Ok(());
    }
    ensure!(
        cfg!(windows) && spec["provider"] == "codex" && spec["driver_kind"] == "codex_app_server",
        "TUI host requires the Windows native Codex app-server driver"
    );
    let root = Path::new(crate::records::string(spec, "tui_host_root")?);
    ensure!(root.is_absolute(), "TUI host root must be absolute");
    for path in root.ancestors() {
        crate::herdr_namespace::unlinked(path)?;
    }
    let argv = spec["command"]
        .as_array()
        .context("missing TUI host command")?;
    ensure!(argv.len() == 3 && argv[1] == "app-server" && argv[2] == "--stdio"
        && argv[0].as_str().is_some_and(|p| Path::new(p).is_absolute() && Path::new(p).is_file()),
        "TUI host requires an absolute Codex executable with app-server --stdio; additional server argv is not supported yet");
    Ok(())
}

fn publish(root: &Path, state: &Value) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(&mut file, state)?;
    file.as_file().sync_all()?;
    file.persist(root.join("host.json"))?;
    Ok(())
}

fn connect(endpoint: &str, token: &str) -> Result<tungstenite::WebSocket<TcpStream>> {
    let address = endpoint
        .strip_prefix("ws://127.0.0.1:")
        .context("host endpoint must be loopback")?;
    let port = address.parse::<u16>()?;
    ensure!(port != 0, "invalid host port");
    let stream =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(250))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut request = endpoint.into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse()?);
    let config = WebSocketConfig::default()
        .max_message_size(Some(LIMIT))
        .max_frame_size(Some(LIMIT));
    let (socket, _) = tungstenite::client::client_with_config(request, stream, Some(config))
        .map_err(|_| anyhow::anyhow!("Codex host websocket handshake failed"))?;
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)))?;
    Ok(socket)
}

fn token(root: &Path) -> Result<String> {
    let path = root.join("token.json");
    crate::security::verify_token(&path)?;
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(crate::records::string(&value, "token")?.to_owned())
}

pub(crate) fn run(root: &Path, executable: &Path) -> Result<()> {
    ensure!(
        cfg!(windows),
        "Codex TUI host currently requires Windows process identity"
    );
    ensure!(
        root.is_absolute() && executable.is_absolute() && executable.is_file(),
        "host requires absolute paths and a Codex executable"
    );
    for path in root.ancestors() {
        crate::herdr_namespace::unlinked(path)?;
    }
    // Each provider lifetime gets a fresh directory. Never take over an old listener.
    fs::create_dir(root).context("Codex host directory must be new")?;
    crate::security::create_token(&root.join("token.json"))?;
    let token = token(root)?;
    let port = TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port();
    let endpoint = format!("ws://127.0.0.1:{port}");
    let mut command = Command::new(executable);
    command.args([
        "app-server",
        "--listen",
        &endpoint,
        "--ws-auth",
        "capability-token",
        "--ws-token-sha256",
        &format!("{:x}", Sha256::digest(token.as_bytes())),
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = crate::owned_process::OwnedChild::spawn(&mut command)?;
    let deadline = Instant::now();
    let mut socket = loop {
        ensure!(
            child.try_wait()?.is_none(),
            "Codex app-server exited during startup"
        );
        if let Ok(socket) = connect(&endpoint, &token) {
            break socket;
        }
        ensure!(
            deadline.elapsed() < Duration::from_secs(15),
            "Codex host startup timed out"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut state = json!({"schema_version":1,"endpoint":endpoint,"executable":executable,
        "cwd":crate::workspaces::portable(&std::env::current_dir()?)?,
        "host_process":crate::owned_process::observe_external_process(std::process::id())?,
        "server_process":crate::owned_process::observe_external_process(child.id())?,
        "thread_id":null,"status":"running"});
    publish(root, &state)?;
    let (sender, input) = mpsc::sync_channel(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            let mut bytes = Vec::new();
            let result = (&mut stdin)
                .take((LIMIT + 1) as u64)
                .read_until(b'\n', &mut bytes);
            match result {
                Ok(0) => break,
                Ok(_) if bytes.len() <= LIMIT && bytes.last() == Some(&b'\n') => {
                    let value = serde_json::from_slice::<Value>(&bytes)
                        .ok()
                        .filter(Value::is_object);
                    let valid = value.is_some();
                    if sender.send(value).is_err() || !valid {
                        break;
                    }
                }
                _ => {
                    let _ = sender.send(None);
                    break;
                }
            }
        }
    });
    let result = (|| -> Result<()> {
        let mut pending = HashSet::new();
        let mut stdout = std::io::stdout().lock();
        loop {
            ensure!(child.try_wait()?.is_none(), "Codex app-server exited");
            for _ in 0..16 {
                match input.try_recv() {
                    Ok(Some(value)) => {
                        if matches!(
                            value["method"].as_str(),
                            Some("thread/start" | "thread/resume")
                        ) {
                            ensure!(
                                pending.len() < 32 && !value["id"].is_null(),
                                "invalid pending thread request"
                            );
                            pending.insert(value["id"].to_string());
                        }
                        socket.send(Message::Text(serde_json::to_string(&value)?.into()))?;
                    }
                    Ok(None) => anyhow::bail!("invalid or oversized Codex input frame"),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let value: Value = serde_json::from_str(&text)?;
                    ensure!(value.is_object(), "non-object Codex response");
                    if pending.remove(&value["id"].to_string()) && value["error"].is_null() {
                        let id = crate::records::string(&value["result"]["thread"], "id")?;
                        uuid::Uuid::parse_str(id)?;
                        state["thread_id"] = json!(id);
                        publish(root, &state)?;
                    }
                    stdout.write_all(text.as_bytes())?;
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => {
                    socket.flush()?;
                }
                Ok(Message::Close(_)) => anyhow::bail!("Codex host connection closed"),
                Ok(_) => anyhow::bail!("unexpected binary Codex frame"),
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => anyhow::bail!("Codex host transport failed; requests were not replayed"),
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    state["status"] = json!("stopped");
    let _ = publish(root, &state);
    result
}

pub(crate) fn attach(root: &Path) -> Result<()> {
    crate::security::own_process_tree()?;
    let state: Value = serde_json::from_slice(&fs::read(root.join("host.json"))?)?;
    ensure!(
        state["schema_version"] == 1 && state["status"] == "running",
        "Codex TUI host is not running"
    );
    for key in ["host_process", "server_process"] {
        let pid = state[key]["pid"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= u32::MAX as u64)
            .context("invalid host process")?;
        ensure!(
            crate::owned_process::observe_external_process(pid as u32)? == state[key],
            "Codex host lifetime changed"
        );
    }
    let thread = crate::records::string(&state, "thread_id")?;
    uuid::Uuid::parse_str(thread)?;
    let endpoint = crate::records::string(&state, "endpoint")?;
    let token = token(root)?;
    // Authenticate before starting a UI; never fall back to a local/new session.
    let mut proof = connect(endpoint, &token)?;
    let _ = proof.close(None);
    let mut child = Command::new(crate::records::string(&state, "executable")?)
        .current_dir(crate::records::string(&state, "cwd")?)
        .args([
            "--cd",
            crate::records::string(&state, "cwd")?,
            "--remote",
            endpoint,
            "--remote-auth-token-env",
            TOKEN_ENV,
            "resume",
            thread,
        ])
        .env(TOKEN_ENV, token)
        .spawn()?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let pid = state["server_process"]["pid"].as_u64().unwrap() as u32;
        if crate::owned_process::observe_external_process(pid)
            .ok()
            .as_ref()
            != Some(&state["server_process"])
        {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Codex TUI host stopped; attachment closed");
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    ensure!(status.success(), "Codex TUI exited unsuccessfully");
    Ok(())
}
