//! Native command surface. No interpreter or shell is used for core requests.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn request(root: &Path, op: &str, payload: Value) -> Result<Value> {
    let endpoint: Value =
        serde_json::from_slice(&std::fs::read(root.join("kernel/control-plane.json"))?)?;
    let token_path = PathBuf::from(
        endpoint["token_ref"]
            .as_str()
            .context("endpoint token reference missing")?,
    );
    let parent = root.join("kernel").canonicalize()?;
    ensure!(
        token_path
            .parent()
            .context("invalid token reference")?
            .canonicalize()?
            == parent,
        "endpoint token escapes state root"
    );
    crate::security::verify_token(&token_path)?;
    let token: Value = serde_json::from_slice(&std::fs::read(&token_path)?)?;
    ensure!(
        token["schema"] == "ccbd-control-plane-token-v1"
            && token["generation"] == endpoint["generation"],
        "endpoint generation mismatch"
    );
    ensure!(
        endpoint["host"] == "127.0.0.1",
        "native client requires loopback endpoint"
    );
    let port = endpoint["port"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 65535)
        .context("invalid endpoint port")?;
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse()?,
        Duration::from_secs(3),
    )?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    write(
        &mut stream,
        &json!({"schema":"ccbd-control-plane-token-v1","token":token["token"]}),
    )?;
    let ack = read(&mut reader)?;
    ensure!(
        ack["schema"] == "ccbd-control-plane-token-ack-v1" && ack["ok"] == true,
        "native authentication rejected"
    );
    write(
        &mut stream,
        &json!({"api_version":2,"op":op,"request":payload,"generation":token["generation"]}),
    )?;
    let response = read(&mut reader)?;
    ensure!(response["api_version"] == 2, "unexpected daemon protocol");
    ensure!(
        response["ok"] == true,
        "{}",
        response["error"]
            .as_str()
            .unwrap_or("daemon request failed")
    );
    Ok(response)
}
fn write(stream: &mut TcpStream, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
}
fn read(reader: &mut BufReader<TcpStream>) -> Result<Value> {
    use std::io::Read;
    let mut bytes = Vec::new();
    reader.take(1024 * 1024 + 1).read_until(b'\n', &mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= 1024 * 1024 && bytes.last() == Some(&b'\n'),
        "invalid native response frame"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn run(mut args: Vec<OsString>) -> Result<()> {
    if args.first().is_some_and(|s| s == "--version") {
        println!("ccb-native {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.is_empty() || args.first().is_some_and(|s| s == "--help") {
        println!("ccb-native --state-root <dir> daemon --config <json>\nccb-native --state-root <dir> ask <agent> --project <id> [--from <actor>] [--chain] [--silence] <message>\nccb-native --state-root <dir> ping|watch <target>|queue [target]|inbox <agent>|stop|request <op> <json>\nccb-native token-create <new-file>");
        return Ok(());
    }
    if args[0] == "token-create" {
        ensure!(args.len() == 2, "token-create requires a new file");
        println!("{}", crate::security::create_token(Path::new(&args[1]))?);
        return Ok(());
    }
    ensure!(
        args.len() >= 3 && args[0] == "--state-root",
        "native command requires --state-root <dir>"
    );
    let root = PathBuf::from(&args[1]);
    args.drain(..2);
    let op = args
        .remove(0)
        .into_string()
        .map_err(|_| anyhow::anyhow!("invalid command encoding"))?;
    if op == "daemon" {
        ensure!(
            args.len() == 2 && args[0] == "--config",
            "daemon requires --config <json>"
        );
        crate::daemon::verify_owner(&root)?;
        let token = root
            .join("kernel")
            .join(format!("token-{}.json", uuid::Uuid::new_v4().simple()));
        crate::security::create_token(&token)?;
        return crate::control_plane::serve_daemon(&root, &token, Path::new(&args[1]), true);
    }
    let payload = match op.as_str() {
        "ping" | "stop" => json!({}),
        "watch" => json!({"target":arg(&args,0)?}),
        "queue" => json!({"target":args.first().and_then(|s|s.to_str()).unwrap_or("all")}),
        "inbox" => json!({"agent_name":arg(&args,0)?}),
        "request" => {
            ensure!(args.len() == 2, "request requires <op> <json>");
            let response = request(&root, arg(&args, 0)?, serde_json::from_str(arg(&args, 1)?)?)?;
            println!("{response}");
            return Ok(());
        }
        "ask" => {
            let target = arg(&args, 0)?.to_owned();
            let mut project = None;
            let mut actor = "user".to_owned();
            let mut chain = false;
            let mut silence = false;
            let mut body = Vec::new();
            let mut i = 1;
            while i < args.len() {
                let value = arg(&args, i)?;
                match value {
                    "--project" => {
                        i += 1;
                        project = Some(arg(&args, i)?.to_owned());
                    }
                    "--from" => {
                        i += 1;
                        actor = arg(&args, i)?.to_owned();
                    }
                    "--chain" => chain = true,
                    "--silence" => silence = true,
                    "--" => {
                        i += 1;
                        while i < args.len() {
                            body.push(arg(&args, i)?.to_owned());
                            i += 1;
                        }
                        break;
                    }
                    _ => body.push(value.to_owned()),
                }
                i += 1;
            }
            json!({"project_id":project.context("ask requires --project <id>")?,"to_agent":target,"from_actor":actor,"body":body.join(" "),"delivery_scope":if target=="all" {"broadcast"} else {"single"},"silence_on_success":silence,"route_options":if chain {json!({"mode":"chain"})} else {json!({})}})
        }
        _ => bail!("unsupported native command: {op}"),
    };
    println!(
        "{}",
        request(
            &root,
            if op == "ask" {
                "submit"
            } else if op == "stop" {
                "shutdown"
            } else {
                &op
            },
            payload
        )?
    );
    Ok(())
}
fn arg(args: &[OsString], index: usize) -> Result<&str> {
    args.get(index)
        .and_then(|s| s.to_str())
        .context("missing or invalid command argument")
}
