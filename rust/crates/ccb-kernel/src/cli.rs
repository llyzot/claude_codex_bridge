//! Native command surface. No interpreter or shell is used for core requests.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Read, Write},
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
    ensure!(
        bytes.len() <= 1024 * 1024,
        "native request frame exceeds 1 MiB"
    );
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
    if args.first().is_some_and(|s| s == "--help") {
        println!("ccb-native [--project <directory>] [--state-root <dir>] start [--executables <manifest>] [--timeout-ms <ms>]\nccb-native [--project <directory>] ask [--compact] [--silence] [--chain] [--artifact-request|--inline-request] [--artifact-reply] <agent> [from <actor>] [--] <message...>\n  target accepts a configured agent or uniquely bound role ID; agents lists bindings\n  ask also reads redirected UTF-8 stdin; --artifact-io enables request/reply artifacts\n  --task-id <id>, --reply-to <id>, --mode <type>; native --from <actor> remains supported\nccb-native --state-root <dir> ask get|cancel <job-id>\nccb-native --state-root <dir> cancel <job-id> [--silent] | retry <job-id>\nccb-native --state-root <dir> watch <target> [--cursor <n>] [--wait --timeout-ms <ms>]\nccb-native --state-root <dir> daemon --config <json>\nccb-native --state-root <dir> agents|ping|queue [target]|inbox <agent>|stop|request <op> <json>\nccb-native --state-root <dir> job-recovery|job-reconcile <job-id>\nccb-native --state-root <dir> provider-status <agent>\nccb-native --state-root <dir> provider-terminate <agent> [--silent]\nccb-native --state-root <dir> provider-recover <agent> <fault-id>\nccb-native --state-root <dir> pending <agent>\nccb-native --state-root <dir> respond <agent> <request-id> <answer-json-file>\nccb-native config compile <project-directory> [<executables-json>]\nccb-native token-create <new-file>");
        return Ok(());
    }
    if args.is_empty() {
        args.push("start".into());
    }
    if args[0] == "private-copy" {
        ensure!(
            args.len() == 3,
            "private-copy requires source and new destination"
        );
        crate::security::copy_private(Path::new(&args[1]), Path::new(&args[2]))?;
        println!("{{\"copied\":true}}");
        return Ok(());
    }
    if args[0] == "config" {
        ensure!(
            args.len() >= 3,
            "config requires show <file> or compile <project-root> <executables-json>"
        );
        let operation = args[1].to_str().context("invalid config operation")?;
        if operation == "show" {
            ensure!(args.len() == 3, "config show requires a file");
            let file = Path::new(&args[2]).canonicalize()?;
            let directory = file.parent().context("config file has no parent")?;
            let root = if directory.file_name().is_some_and(|n| n == ".ccb") {
                directory.parent().unwrap_or(directory)
            } else {
                directory
            };
            let document = crate::config::parse_at(&std::fs::read_to_string(&file)?, Some(root))?;
            println!("{document}");
            return Ok(());
        }
        ensure!(
            operation == "compile" && (args.len() == 3 || args.len() == 4),
            "config compile requires project root and optional executable manifest"
        );
        let root = Path::new(&args[2]).canonicalize()?;
        let document = crate::config::parse_at(
            &std::fs::read_to_string(root.join(".ccb/ccb.config"))?,
            Some(&root),
        )?;
        let executables: Value = if args.len() == 4 {
            serde_json::from_slice(&std::fs::read(Path::new(&args[3]))?)?
        } else {
            crate::executables::discover(&document)?
        };
        println!("{}", crate::config::compile(&root, document, &executables)?);
        return Ok(());
    }
    if args[0] == "token-create" {
        ensure!(args.len() == 2, "token-create requires a new file");
        println!("{}", crate::security::create_token(Path::new(&args[1]))?);
        return Ok(());
    }
    let mut explicit_root = None;
    let mut project = None;
    while args
        .first()
        .is_some_and(|a| a == "--state-root" || a == "--project")
    {
        ensure!(args.len() >= 2, "global option requires a directory");
        if args[0] == "--state-root" {
            explicit_root = Some(PathBuf::from(&args[1]));
        } else {
            project = Some(crate::config::project_root(Path::new(&args[1]))?);
        }
        args.drain(..2);
    }
    let inherited_root = if explicit_root.is_none() && project.is_none() {
        std::env::var_os("CCB_NATIVE_TOKEN_FILE").and_then(|token| {
            let token = PathBuf::from(token);
            let kernel = token.parent()?;
            (token.is_absolute() && kernel.file_name()? == "kernel")
                .then(|| kernel.parent().map(Path::to_path_buf))
                .flatten()
        })
    } else {
        None
    };
    if project.is_none() && explicit_root.is_none() && inherited_root.is_none() {
        project = Some(crate::config::project_root(&std::env::current_dir()?)?);
    }
    let root = explicit_root
        .or(inherited_root)
        .map(Ok)
        .unwrap_or_else(|| {
            crate::config::state_root(
                project
                    .as_deref()
                    .context("native command requires a project or --state-root")?,
            )
        })?;
    if args.is_empty() {
        args.push("start".into());
    }
    let mut op = args
        .remove(0)
        .into_string()
        .map_err(|_| anyhow::anyhow!("invalid command encoding"))?;
    if op == "ask" && args.first().is_some_and(|s| s == "get" || s == "cancel") {
        op = if args.remove(0) == "get" {
            "watch".into()
        } else {
            "cancel".into()
        };
    }
    if op == "daemon" {
        let config = if args.len() == 1 && args[0] == "--config-stdin" {
            let mut bytes = Vec::new();
            std::io::stdin()
                .lock()
                .take(1024 * 1024 + 2)
                .read_until(b'\n', &mut bytes)?;
            ensure!(
                bytes.len() <= 1024 * 1024 + 1 && bytes.last() == Some(&b'\n'),
                "invalid native bootstrap frame"
            );
            serde_json::from_slice::<Value>(&bytes)?
        } else {
            ensure!(
                args.len() == 2 && args[0] == "--config",
                "daemon requires --config <json>"
            );
            serde_json::from_slice::<Value>(&std::fs::read(Path::new(&args[1]))?)?
        };
        crate::daemon::Daemon::validate_config(&config)?;
        crate::daemon::verify_owner(&root)?;
        let token = root
            .join("kernel")
            .join(format!("token-{}.json", uuid::Uuid::new_v4().simple()));
        crate::security::create_token(&token)?;
        return crate::control_plane::serve_config(&root, &token, config);
    }
    if op == "start" {
        let mut manifest = None;
        let mut config_file = None;
        let mut timeout = 15_000u64;
        let mut i = 0;
        while i < args.len() {
            match arg(&args, i)? {
                "--executables" => {
                    i += 1;
                    manifest = Some(PathBuf::from(
                        args.get(i).context("--executables requires a file")?,
                    ));
                }
                "--config" => {
                    i += 1;
                    config_file = Some(PathBuf::from(
                        args.get(i).context("--config requires a file")?,
                    ));
                }
                "--timeout-ms" => {
                    i += 1;
                    timeout = arg(&args, i)?.parse()?;
                }
                other => bail!("unknown start option: {other}"),
            }
            i += 1;
        }
        let config = if let Some(path) = config_file {
            ensure!(
                manifest.is_none(),
                "--config cannot be combined with --executables"
            );
            serde_json::from_slice(&std::fs::read(path)?)?
        } else {
            let project = project
                .clone()
                .map(Ok)
                .unwrap_or_else(|| crate::config::project_root(&std::env::current_dir()?))?;
            let document = crate::config::parse_at(
                &std::fs::read_to_string(project.join(".ccb/ccb.config"))?,
                Some(&project),
            )?;
            let executables = if let Some(path) = manifest {
                serde_json::from_slice(&std::fs::read(path)?)?
            } else {
                crate::executables::discover(&document)?
            };
            crate::config::compile(&project, document, &executables)?
        };
        println!(
            "{}",
            crate::startup::start(&root, config, Duration::from_millis(timeout))?
        );
        return Ok(());
    }
    let payload = match op.as_str() {
        "ping" | "stop" | "agents" => {
            ensure!(args.is_empty(), "command takes no arguments");
            json!({})
        }
        "watch" | "get" | "pend" => {
            return crate::cli_tasks::watch(&root, &args);
        }
        "cancel" => {
            ensure!(
                args.len() == 1 || args.len() == 2 && arg(&args, 1)? == "--silent",
                "cancel requires Job ID and optional --silent"
            );
            json!({"job_id":arg(&args,0)?,"record_reply":args.len()==1})
        }
        "retry" => {
            ensure!(args.len() == 1, "retry requires a Job ID");
            json!({"target":arg(&args,0)?})
        }
        "job-recovery" | "job-reconcile" => {
            ensure!(args.len() == 1, "job recovery requires a target Job");
            json!({"target":arg(&args,0)?})
        }
        "queue" => json!({"target":args.first().and_then(|s|s.to_str()).unwrap_or("all")}),
        "inbox" => json!({"agent_name":arg(&args,0)?}),
        "provider-status" => {
            ensure!(args.len() == 1, "provider-status requires an agent");
            json!({"agent_name":arg(&args,0)?})
        }
        "provider-terminate" => {
            ensure!(
                args.len() == 1 || args.len() == 2 && arg(&args, 1)? == "--silent",
                "provider-terminate requires agent and optional --silent"
            );
            let agent = arg(&args, 0)?;
            let current = request(&root, "provider-status", json!({"agent_name":agent}))?;
            let prior = &current["fault"]["termination_intent"]["binding"];
            let mut binding = if !prior.is_null() {
                prior.clone()
            } else {
                json!({"runtime_instance_id":current["owned"]["runtime_instance_id"],"pid":current["owned"]["pid"],"scope_id":current["scope"]["scope_id"],
                    "job_id":current["active"]["job_id"],"execution_instance_id":current["active"]["execution_instance_id"]})
            };
            binding["agent_name"] = json!(agent);
            binding["record_reply"] = json!(args.len() == 1);
            println!("{}", request(&root, "provider-terminate", binding)?);
            return Ok(());
        }
        "provider-recover" => {
            ensure!(
                args.len() == 2,
                "provider-recover requires agent and fault identity"
            );
            json!({"agent_name":arg(&args,0)?,"fault_id":arg(&args,1)?})
        }
        "pending" => {
            ensure!(args.len() == 1, "pending requires an agent");
            println!(
                "{}",
                request_preview(&request(
                    &root,
                    "provider-requests",
                    json!({"agent_name":arg(&args,0)?})
                )?)
            );
            return Ok(());
        }
        "respond" => {
            ensure!(
                args.len() == 3,
                "respond requires agent, request handle and answer JSON file"
            );
            let agent = arg(&args, 0)?;
            let handle = arg(&args, 1)?;
            let current = request(&root, "provider-requests", json!({"agent_name":agent}))?;
            let pending = current["requests"]
                .as_array()
                .context("missing pending requests")?
                .iter()
                .find(|r| r["request_id"] == handle)
                .context("unknown or resolved request handle")?;
            let mut bytes = Vec::new();
            std::fs::File::open(Path::new(&args[2]))?
                .take(512 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 512 * 1024, "answer file is too large");
            let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
            let response = json!({"agent_name":agent,"request_id":handle,
                "runtime_instance_id":pending["runtime_instance_id"],"job_id":pending["job_id"],
                "execution_instance_id":pending["execution_instance_id"],
                "result":serde_json::from_slice::<Value>(bytes)?});
            println!("{}", request(&root, "provider-respond", response)?);
            return Ok(());
        }
        "request" => {
            ensure!(args.len() == 2, "request requires <op> <json>");
            let response = request(&root, arg(&args, 0)?, serde_json::from_str(arg(&args, 1)?)?)?;
            println!("{response}");
            return Ok(());
        }
        "ask" => {
            let mut payload = crate::cli_tasks::ask(&args)?;
            if payload["project_id"].is_null() {
                payload["project_id"] = if let Some(project) = project.as_deref() {
                    json!(crate::config::project_id(project)?)
                } else {
                    request(&root, "ping", json!({}))?["project_id"].clone()
                };
            }
            payload
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
// Preserve exact JSON values while making terminal formatting controls visible.
fn request_preview(value: &Value) -> String {
    let mut text = String::new();
    for ch in value.to_string().chars() {
        if ch.is_control()
            || matches!(ch,'\u{061c}'|'\u{200b}'..='\u{200f}'|'\u{2028}'..='\u{202e}'|'\u{2060}'|'\u{2066}'..='\u{2069}'|'\u{feff}')
        {
            text.push_str(&format!("\\u{:04x}", ch as u32));
        } else {
            text.push(ch);
        }
    }
    text
}
fn arg(args: &[OsString], index: usize) -> Result<&str> {
    args.get(index)
        .and_then(|s| s.to_str())
        .context("missing or invalid command argument")
}
