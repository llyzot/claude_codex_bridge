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
    // The Windows launcher forwards ask.exe metadata queries as `ask <flag>`.
    // They must work before project discovery, just like the native ask alias.
    if args.first().is_some_and(|s| s == "ask")
        && args.get(1).is_some_and(|s| {
            ["--help", "--version", "--print-version", "--capabilities"]
                .iter()
                .any(|flag| s == flag)
        })
    {
        args.remove(0);
    }
    if args.first().is_some_and(|s| s == "--capabilities") {
        ensure!(args.len() == 1, "--capabilities takes no arguments");
        println!(
            "{}",
            json!({"schema_version":1,"service":"ccb-native","runtime":"rust",
            "version":crate::installation::version(),"core_version":env!("CARGO_PKG_VERSION"),
            "public_api_version":2,"herdr_open":{"experimental":true,"supported":cfg!(windows),"terminal_providers":["pi","codex"],"background_startup":true},"native_prefix_installation":{"schema_version":1,
                "supported":cfg!(all(windows,target_arch="x86_64")),"commands":["ccb","ask","ccb-native"],
                "modifies_account_settings":false,"in_place_upgrade":true,"upgrade_requires_stopped_commands":true,
                "upgrade_recovery":true,"program_file_rollback":true,"runtime_state_rollback":false}})
        );
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|s| s == "--version" || s == "--print-version")
    {
        println!(
            "ccb-native {} (core {})",
            crate::installation::version(),
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "--help") {
        println!("ccb-native [--project <directory>] [--state-root <dir>] start [--executables <manifest>] [--timeout-ms <ms>]\nccb-native [--project <directory>] ask [--compact] [--silence] [--chain] [--artifact-request|--inline-request] [--artifact-reply] <agent> [from <actor>] [--] <message...>\n  target accepts a configured agent or uniquely bound role ID; agents lists bindings\n  ask also reads redirected UTF-8 stdin; --artifact-io enables request/reply artifacts\n  --task-id <id>, --reply-to <id>, --mode <type>; native --from <actor> remains supported\nccb-native --state-root <dir> ask get|cancel <job-id>\nccb-native --state-root <dir> cancel <job-id> [--silent] | retry <job-id>\nccb-native --state-root <dir> watch <target> [--cursor <n>] [--wait --timeout-ms <ms>]\nccb-native --state-root <dir> daemon --config <json>\nccb-native --state-root <dir> agents|ping|queue [target]|inbox <agent>|stop|request <op> <json>\nccb-native --state-root <dir> job-recovery|job-reconcile <job-id>\nccb-native --state-root <dir> provider-status <agent>\nccb-native --state-root <dir> provider-terminate <agent> [--silent]\nccb-native --state-root <dir> provider-recover <agent> <fault-id>\nccb-native --state-root <dir> pending <agent>\nccb-native --state-root <dir> respond <agent> <request-id> <answer-json-file>\nccb-native workspace-tree <path> [--include-controller-state]\nccb-native config compile <project-directory> [<executables-json>]\nccb-native token-create <new-file>");
        println!("ccb-native [--project <directory>] [--state-root <dir>] reload [--config <json>|--executables <manifest>] [--dry-run] [--wait --timeout-ms <ms>]\nccb-native --state-root <dir> reload-status");
        println!("ccb-native install --prefix <new-absolute-directory> [--dry-run]\nccb-native installation verify --prefix <directory>");
        println!("ccb-native installation upgrade|recover|rollback --prefix <directory> [--dry-run] [--operation-id <uuid>] [--expected-manifest <sha256>]\nccb-native installation status --prefix <directory>");
        println!("ccb-native --state-root <dir> view [--limit <1..64>]");
        println!("ccb-native [--project <directory>] herdr open [--herdr-exe <path>] [--session <name>] [--options <json>] [--no-attach] [--timeout-ms <0..60000>] [--dry-run]");
        println!("ccb-native --state-root <dir> clear|compact [agent|role...|all] [--dry-run] [--operation-id <uuid>] [--wait --timeout-ms <ms>]\nccb-native --state-root <dir> context-status <operation-id>");
        println!("ccb-native --state-root <dir> restart [agent|role...|all] [--dry-run] [--operation-id <uuid>] [--wait --timeout-ms <ms>]\nccb-native --state-root <dir> restart-status [operation-id]");
        return Ok(());
    }
    if std::env::current_exe()?
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("ask"))
    {
        let mut prefix = 0;
        while args
            .get(prefix)
            .is_some_and(|s| s == "--state-root" || s == "--project")
        {
            ensure!(
                args.get(prefix + 1).is_some(),
                "ask directory option requires a value"
            );
            prefix += 2;
        }
        args.insert(prefix, "ask".into());
    }
    if args
        .first()
        .is_some_and(|s| s == "install" || s == "installation")
    {
        if args[0] == "installation" && args.get(1).is_some_and(|s| s != "verify") {
            let action = arg(&args, 1)?;
            ensure!(
                ["upgrade", "recover", "rollback", "status"].contains(&action),
                "unknown installation action"
            );
            let (mut prefix, mut operation, mut expected) = (None, None, None);
            let mut dry_run = false;
            let mut i = 2;
            while i < args.len() {
                match arg(&args, i)? {
                    "--prefix" => {
                        i += 1;
                        prefix = Some(PathBuf::from(args.get(i).context("missing prefix")?));
                    }
                    "--operation-id" if action != "status" => {
                        i += 1;
                        operation = Some(arg(&args, i)?);
                    }
                    "--expected-manifest" if action != "status" => {
                        i += 1;
                        expected = Some(arg(&args, i)?);
                    }
                    "--dry-run" if action != "status" => dry_run = true,
                    option => bail!("unknown installation option: {option}"),
                }
                i += 1;
            }
            let prefix = prefix.context("installation management requires --prefix")?;
            let result = if action == "status" {
                crate::installation::status(&prefix)?
            } else {
                crate::installation::manage(action, &prefix, dry_run, operation, expected)?
            };
            println!("{result}");
            return Ok(());
        }
        let verify = args[0] == "installation";
        let offset = if verify { 2 } else { 1 };
        if verify {
            ensure!(
                args.get(1).is_some_and(|s| s == "verify"),
                "installation requires verify"
            );
        }
        ensure!(
            args.get(offset).is_some_and(|s| s == "--prefix"),
            "installation requires --prefix <directory>"
        );
        let prefix = Path::new(
            args.get(offset + 1)
                .context("missing installation prefix")?,
        );
        let dry_run = !verify && args.get(offset + 2).is_some_and(|s| s == "--dry-run");
        ensure!(
            args.len() == offset + 2 + usize::from(dry_run),
            "unexpected installation option"
        );
        println!(
            "{}",
            if verify {
                crate::installation::verify(prefix)?
            } else {
                crate::installation::install(prefix, dry_run)?
            }
        );
        return Ok(());
    }
    crate::installation::check_command_installation()?;
    if args.first().is_some_and(|s| s == "--herdr-open-worker") {
        ensure!(args.len() == 1, "invalid Herdr worker arguments");
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024 && bytes.last() == Some(&b'\n'),
            "invalid Herdr worker bootstrap frame"
        );
        return crate::herdr_open::worker(&serde_json::from_slice(&bytes)?);
    }
    if args.is_empty() {
        args.push("start".into());
    }
    if args[0] == "herdr-transport"
        || args[0] == "herdr-namespace"
        || args[0] == "herdr-agents"
        || args[0] == "pi-terminal-bridge"
    {
        ensure!(
            args.len() == 1,
            "herdr-transport reads one JSON request from stdin"
        );
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "Herdr transport request exceeds 1 MiB"
        );
        let request: Value = serde_json::from_slice(&bytes)?;
        crate::security::own_process_tree()?;
        println!(
            "{}",
            if args[0] == "pi-terminal-bridge" {
                crate::pi_terminal::execute(&request)?
            } else if args[0] == "herdr-transport" {
                crate::herdr_transport::execute(&request)?
            } else if args[0] == "herdr-agents" {
                crate::herdr_agents::execute(&request)?
            } else {
                crate::herdr_namespace::execute(&request)?
            }
        );
        return Ok(());
    }
    if args[0] == "process-identity" {
        ensure!(args.len() == 2, "process-identity requires one PID");
        let pid: u32 = args[1].to_str().context("invalid process PID")?.parse()?;
        println!("{}", crate::owned_process::observe_external_process(pid)?);
        return Ok(());
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
    if args[0] == "workspace-tree" {
        crate::security::own_process_tree()?;
        ensure!(
            args.len() == 2 || args.len() == 3 && args[2] == "--include-controller-state",
            "workspace-tree requires a path and optional --include-controller-state"
        );
        println!(
            "{}",
            crate::git_observation::current(Path::new(&args[1]), args.len() == 2)?
        );
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
        std::env::var_os("CCB_NATIVE_TOKEN_FILE")
            .and_then(|token| {
                let token = PathBuf::from(token);
                let kernel = token.parent()?;
                (token.is_absolute() && kernel.file_name()? == "kernel")
                    .then(|| kernel.parent().map(Path::to_path_buf))
                    .flatten()
            })
            .or_else(|| {
                std::env::var_os("CCB_NATIVE_STATE_ROOT")
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
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
    if op == "view" || op == "project-view" {
        op = "project_view".into();
    }
    if op == "ask" && args.first().is_some_and(|s| s == "get" || s == "cancel") {
        op = if args.remove(0) == "get" {
            "watch".into()
        } else {
            "cancel".into()
        };
    }
    if op == "herdr" {
        use std::io::IsTerminal;
        ensure!(
            args.first().is_some_and(|v| v == "open"),
            "native herdr requires open"
        );
        let project = project
            .as_deref()
            .context("herdr open requires a project, not an alternate --state-root")?;
        let project_path = PathBuf::from(crate::workspaces::portable(project)?);
        let project = project_path.as_path();
        let mut options = json!({});
        let mut no_attach = false;
        let mut timeout = 10000;
        let mut index = 1;
        while index < args.len() {
            match args[index].to_str().context("invalid Herdr option")? {
                "--options" => {
                    index += 1;
                    options = serde_json::from_slice(&std::fs::read(Path::new(
                        args.get(index).context("--options requires a JSON file")?,
                    ))?)?;
                    ensure!(options.is_object(), "Herdr options must be an object");
                }
                "--herdr-exe" => {
                    index += 1;
                    options["command"] = json!([arg(&args, index)?]);
                }
                "--session" | "--herdr-session" => {
                    index += 1;
                    options["session"] = json!(arg(&args, index)?);
                }
                "--timeout-ms" => {
                    index += 1;
                    timeout = arg(&args, index)?.parse()?;
                }
                "--no-attach" => no_attach = true,
                "--dry-run" => options["dry_run"] = json!(true),
                other => bail!("unknown herdr open option: {other}"),
            }
            index += 1;
        }
        ensure!(timeout <= 60000, "Herdr open observation limit is 60000ms");
        let config = if let Some(config) = options.get("config") {
            config.clone()
        } else {
            let source = crate::config::parse_at(
                &std::fs::read_to_string(project.join(".ccb/ccb.config"))?,
                Some(project),
            )?;
            crate::config::compile(
                project,
                source,
                &json!({"pi":["pi"],"codex":["codex"],"claude":["claude"]}),
            )?
        };
        let result =
            crate::herdr_open::launch(project, config, &options, Duration::from_millis(timeout))?;
        println!("{result}");
        if !no_attach
            && options["dry_run"] != true
            && result["workspace_id"].is_string()
            && std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
        {
            crate::herdr_open::attach(&options, &result)?;
        }
        return Ok(());
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
    if op == "start" || op == "reload" {
        let mut manifest = None;
        let mut config_file = None;
        let mut timeout = 15_000u64;
        let mut dry_run = false;
        let mut wait = false;
        let mut i = 0;
        while i < args.len() {
            match arg(&args, i)? {
                "--dry-run" if op == "reload" => dry_run = true,
                "--wait" if op == "reload" => wait = true,
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
                other => bail!("unknown {op} option: {other}"),
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
        if op == "reload" {
            let plan = request(&root, "reload-plan", json!({"config":config}))?;
            if dry_run {
                println!("{plan}");
                return Ok(());
            }
            let mut result = request(
                &root,
                "reload",
                json!({"config":config,
                "expected_fingerprint":plan["plan"]["current_fingerprint"]}),
            )?;
            let operation = result["reload"]["operation_id"].clone();
            let deadline = std::time::Instant::now() + Duration::from_millis(timeout);
            while wait
                && ["preparing", "retiring"]
                    .contains(&result["reload"]["status"].as_str().unwrap_or(""))
            {
                if std::time::Instant::now() >= deadline {
                    result["observation_timeout"] = json!(true);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
                result = request(&root, "reload-status", json!({}))?;
                ensure!(
                    result["reload"]["operation_id"] == operation,
                    "reload observation superseded; inspect reload-status"
                );
            }
            println!("{result}");
            ensure!(
                !["blocked", "failed", "interrupted"]
                    .contains(&result["reload"]["status"].as_str().unwrap_or("")),
                "configuration reload was not applied; inspect the returned status"
            );
            return Ok(());
        }
        println!(
            "{}",
            crate::startup::start(&root, config, Duration::from_millis(timeout))?
        );
        return Ok(());
    }
    if op == "restart" {
        let mut targets = Vec::new();
        let mut operation = uuid::Uuid::new_v4().to_string();
        let (mut dry_run, mut wait, mut timeout) = (false, false, 15_000u64);
        let mut i = 0;
        while i < args.len() {
            match arg(&args, i)? {
                "--dry-run" => dry_run = true,
                "--wait" => wait = true,
                "--operation-id" => {
                    i += 1;
                    operation = arg(&args, i)?.to_owned();
                }
                "--timeout-ms" => {
                    i += 1;
                    timeout = arg(&args, i)?.parse()?;
                }
                other if other.starts_with('-') => bail!("unknown restart option: {other}"),
                name => targets.push(name.to_owned()),
            }
            i += 1;
        }
        ensure!(
            timeout <= 86_400_000,
            "restart observation timeout exceeds one day"
        );
        ensure!(
            uuid::Uuid::parse_str(&operation).is_ok(),
            "invalid restart operation identity"
        );
        let mut result = request(
            &root,
            "restart",
            json!({"agent_names":targets,
            "operation_id":operation,"dry_run":dry_run}),
        ).with_context(|| format!("restart request {operation} failed; inspect restart-status {operation} before retrying"))?;
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout);
        while wait
            && !dry_run
            && ["preparing", "retiring"]
                .contains(&result["reload"]["status"].as_str().unwrap_or(""))
        {
            if std::time::Instant::now() >= deadline {
                result["observation_timeout"] = json!(true);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            result = request(&root, "restart-status", json!({"operation_id":operation}))
                .with_context(|| {
                    format!("restart observation failed; resume restart-status {operation}")
                })?;
            ensure!(
                result["reload"]["operation_id"] == operation,
                "restart receipt unavailable; inspect restart-status with the operation ID"
            );
        }
        println!("{result}");
        ensure!(
            !["blocked", "failed", "interrupted"]
                .contains(&result["reload"]["status"].as_str().unwrap_or("")),
            "provider restart was not applied; inspect the returned status"
        );
        return Ok(());
    }
    if op == "clear" || op == "compact" {
        return crate::cli_context::run(&root, &op, &args);
    }
    let payload = match op.as_str() {
        "context-status" => {
            ensure!(args.len() == 1, "context-status requires an operation ID");
            json!({"operation_id":arg(&args,0)?})
        }
        "restart-status" => {
            ensure!(args.len() <= 1, "restart-status accepts one operation ID");
            if args.is_empty() {
                json!({})
            } else {
                json!({"operation_id":arg(&args,0)?})
            }
        }
        "project_view" => {
            ensure!(
                args.is_empty() || args.len() == 2 && arg(&args, 0)? == "--limit",
                "view accepts only --limit <1..64>"
            );
            json!({"schema_version":1,"comms_limit":if args.is_empty(){8}else{arg(&args,1)?.parse::<u64>()?}})
        }
        "ping" | "stop" | "agents" | "reload-status" => {
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
