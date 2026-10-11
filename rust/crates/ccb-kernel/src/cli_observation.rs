//! Legacy pend/queue syntax over non-mutating native observations.
use anyhow::{bail, ensure, Context, Result};
use serde_json::json;
use std::{ffi::OsString, path::Path};
pub(crate) fn run(root: &Path, op: &str, args: &[OsString]) -> Result<()> {
    let mut mode = None;
    let mut detail = false;
    let mut positional = Vec::new();
    let mut watch = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_str().context("invalid observer argument")?;
        if arg.starts_with('-') {
            ensure!(seen.insert(arg), "duplicate observer option");
            match arg {
                "--detail" => detail = true,
                "--inbox" | "--queue" | "--watch" | "--wait" if op == "pend" => {
                    ensure!(mode.is_none(), "pend supports one observer mode");
                    mode = Some(if arg == "--wait" { "--watch" } else { arg });
                }
                "--cursor" | "--timeout-ms" if op == "pend" => {
                    watch.push(args[i].clone());
                    i += 1;
                    let value = args.get(i).context("missing observer option value")?;
                    value
                        .to_str()
                        .context("invalid observer value")?
                        .parse::<u64>()?;
                    watch.push(value.clone());
                }
                _ => bail!("unknown observer option: {arg}"),
            }
        } else {
            positional.push(arg.to_owned());
        }
        i += 1;
    }
    if op == "queue" {
        ensure!(positional.len() <= 1, "queue accepts one target");
        let target = positional.first().map(String::as_str).unwrap_or("all");
        println!(
            "{}",
            crate::cli::request_preview(&crate::cli::request(
                root,
                "queue",
                json!({"target":target,"detail":detail})
            )?)
        );
        return Ok(());
    }
    ensure!(
        !positional.is_empty() && positional.len() <= 2,
        "pend requires a target and optional positive legacy count"
    );
    if positional.len() == 2 {
        ensure!(
            mode.is_none() && watch.is_empty(),
            "pend count is only supported for snapshot mode"
        );
        ensure!(
            positional[1].parse::<u64>()? > 0,
            "pend count must be positive"
        );
    }
    ensure!(
        !detail || matches!(mode, Some("--inbox" | "--queue")),
        "pend --detail requires --inbox or --queue"
    );
    let target = &positional[0];
    if mode == Some("--watch") || !watch.is_empty() {
        ensure!(
            mode.is_none() || mode == Some("--watch"),
            "watch options require watch mode"
        );
        let mut values = vec![OsString::from(target)];
        if mode == Some("--watch") {
            values.push("--wait".into());
        }
        values.extend(watch);
        return crate::cli_tasks::watch(root, &values);
    }
    let (operation, payload) = match mode {
        Some("--inbox") => ("inbox", json!({"agent_name":target,"detail":detail})),
        Some("--queue") => ("queue", json!({"target":target,"detail":detail})),
        _ => ("pend", json!({"target":target})),
    };
    // Legacy count is a parser compatibility argument: its old service also
    // returns one latest-job/mailbox snapshot, not a history selection.
    println!(
        "{}",
        crate::cli::request_preview(&crate::cli::request(root, operation, payload)?)
    );
    Ok(())
}
