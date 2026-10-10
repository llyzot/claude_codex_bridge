//! Authenticated loopback serving. Peer reads are bounded; one actor writes.
use crate::{rpc, Kernel};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
const AUTH_SCHEMA: &str = "ccbd-control-plane-token-v1";
const ACK_SCHEMA: &str = "ccbd-control-plane-token-ack-v1";
struct Command {
    line: String,
    reply: SyncSender<Value>,
}
enum Event {
    Request(Command),
    Stop,
}
struct PeerPermit(Arc<AtomicUsize>);
impl Drop for PeerPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
pub fn serve(root: &Path, token_path: &Path) -> Result<()> {
    ensure!(
        token_path
            .parent()
            .context("missing token parent")?
            .canonicalize()?
            == root.canonicalize()?.join("kernel"),
        "token must belong to kernel state directory"
    );
    let token: Value = serde_json::from_slice(&fs::read(token_path)?)?;
    ensure!(token["schema"] == AUTH_SCHEMA, "invalid token schema");
    let expected = token["token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing token")?
        .to_owned();
    let generation = token["generation"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing token generation")?
        .to_owned();
    let mut kernel = Kernel::open(root)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let stop = Arc::new(AtomicBool::new(false));
    let owner_stop = stop.clone();
    let (commands, requests) = mpsc::sync_channel::<Event>(64);
    let owner_commands = commands.clone();
    // Creator pipe EOF fences ownership without relying on a reusable PID.
    thread::spawn(move || {
        let mut bytes = [0u8; 256];
        let mut input = io::stdin().lock();
        while input.read(&mut bytes).is_ok_and(|n| n > 0) {}
        owner_stop.store(true, Ordering::Release);
        let _ = owner_commands.try_send(Event::Stop);
    });
    let peers = Arc::new(AtomicUsize::new(0));
    let ready = json!({"service":"ccb-kernel","transport":"tcp_loopback","host":"127.0.0.1","port":listener.local_addr()?.port(),"generation":generation,"pid":std::process::id()});
    let mut descriptor = ready.clone();
    descriptor["token_ref"] = json!(token_path);
    let descriptor_path = root.join("kernel/control-plane.json");
    let temporary = descriptor_path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec(&descriptor)?)?;
    fs::rename(&temporary, &descriptor_path)?;
    writeln!(io::stdout(), "{ready}")?;
    io::stdout().flush()?;
    let accepting_stop = stop.clone();
    let accepting_generation = generation.clone();
    thread::spawn(move || {
        while !accepting_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    if !peer.ip().is_loopback() || peers.load(Ordering::Acquire) >= 16 {
                        drop(stream);
                        continue;
                    }
                    peers.fetch_add(1, Ordering::AcqRel);
                    let permit = PeerPermit(peers.clone());
                    let sender = commands.clone();
                    let token = expected.clone();
                    let generation = accepting_generation.clone();
                    let stopping = accepting_stop.clone();
                    thread::spawn(move || {
                        let _permit = permit;
                        if let Err(error) =
                            connection(stream, &token, &generation, &sender, &stopping)
                        {
                            eprintln!("control-plane peer rejected: {error}");
                        }
                    });
                }
                Err(_) => {
                    accepting_stop.store(true, Ordering::Release);
                    let _ = commands.try_send(Event::Stop);
                    break;
                }
            }
        }
    });
    while !stop.load(Ordering::Acquire) {
        if let Ok(event) = requests.recv_timeout(Duration::from_secs(1)) {
            let command = match event {
                Event::Stop => break,
                Event::Request(command) => command,
            };
            let response = rpc::execute(&mut kernel, &command.line)?;
            let _ = command.reply.send(response);
        }
    }
    // Only this exact generation may remove its published endpoint.
    if fs::read(&descriptor_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|d| d["generation"] == generation)
    {
        let _ = fs::remove_file(&descriptor_path);
    }
    Ok(())
}
fn connection(
    mut stream: TcpStream,
    token: &str,
    generation: &str,
    commands: &SyncSender<Event>,
    stop: &AtomicBool,
) -> Result<()> {
    // Windows accepted sockets inherit the listener's nonblocking mode.
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let mut buffered = Vec::new();
    let auth = read_line(&mut stream, &mut buffered, 4096, Duration::from_millis(500))?;
    let auth: Value = serde_json::from_slice(&auth)?;
    ensure!(auth["schema"] == AUTH_SCHEMA, "invalid auth schema");
    let supplied = auth["token"].as_str().context("missing auth token")?;
    ensure!(
        bool::from(Sha256::digest(supplied.as_bytes()).ct_eq(&Sha256::digest(token.as_bytes()))),
        "auth rejected"
    );
    send(&mut stream, &json!({"schema":ACK_SCHEMA,"ok":true}))?;
    let raw = read_line(
        &mut stream,
        &mut buffered,
        1024 * 1024,
        Duration::from_millis(500),
    )?;
    let value: Value = serde_json::from_slice(&raw)?;
    ensure!(value["generation"] == generation, "stale kernel generation");
    ensure!(!stop.load(Ordering::Acquire), "kernel stopping");
    let line = String::from_utf8(raw)?;
    let (reply, result) = mpsc::sync_channel(1);
    commands
        .try_send(Event::Request(Command { line, reply }))
        .context("control plane request queue full")?;
    let response = result
        .recv_timeout(Duration::from_secs(10))
        .context("control plane dispatch timed out")?;
    send(&mut stream, &response)
}
fn send(stream: &mut TcpStream, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
}
fn read_line(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(index) = buffer.iter().position(|b| *b == b'\n') {
            ensure!(index <= limit, "control plane line too large");
            let remaining = buffer.split_off(index + 1);
            let mut line = std::mem::replace(buffer, remaining);
            line.pop();
            return Ok(line);
        }
        ensure!(buffer.len() <= limit, "control plane line too large");
        let left = deadline
            .checked_duration_since(Instant::now())
            .context("control plane read timed out")?;
        stream.set_read_timeout(Some(left))?;
        let mut bytes = [0u8; 4096];
        let count = stream.read(&mut bytes)?;
        if count == 0 {
            bail!("control plane peer disconnected");
        }
        buffer.extend_from_slice(&bytes[..count]);
    }
}
