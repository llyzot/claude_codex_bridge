//! Resolve native provider programs without running a shell or package manager.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    path::{Path, PathBuf},
};

fn executable(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        for suffix in if cfg!(windows) {
            vec![".exe", ""]
        } else {
            vec![""]
        } {
            let candidate = dir.join(format!("{name}{suffix}"));
            if candidate.is_file() {
                return Some(resolve_shim(candidate));
            }
        }
    }
    None
}
fn resolve_shim(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        if let Ok(text) = std::fs::read_to_string(path.with_extension("shim")) {
            for line in text.lines() {
                if let Some(value) = line.trim().strip_prefix("path = ") {
                    let target = PathBuf::from(value.trim_matches('"'));
                    if target.is_absolute() && target.is_file() {
                        return target;
                    }
                }
            }
        }
    }
    path
}
fn package_roots(node: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(parent) = node.and_then(Path::parent) {
        roots.extend([
            parent.join("node_modules"),
            parent.join("bin/node_modules"),
            parent.join("../lib/node_modules"),
        ]);
    }
    if let Some(home) = env::var_os("APPDATA") {
        roots.push(PathBuf::from(home).join("npm/node_modules"));
    }
    if let Some(paths) = env::var_os("NODE_PATH") {
        roots.extend(env::split_paths(&paths));
    }
    roots
}
fn provider(provider: &str) -> Result<Value> {
    let node = executable("node");
    let roots = package_roots(node.as_deref());
    if provider == "pi" {
        let node = node.context("Pi requires an existing Node executable on PATH")?;
        for root in roots {
            let path = root.join("@earendil-works/pi-coding-agent/dist/bundle/cli.js");
            if path.is_file() {
                return Ok(json!([node, path]));
            }
        }
        bail!("Pi package entry was not found beside Node; supply --executables or install Pi separately");
    }
    if provider == "codex" && cfg!(windows) {
        let (package, triple) = if cfg!(target_arch = "aarch64") {
            ("codex-win32-arm64", "aarch64-pc-windows-msvc")
        } else {
            ("codex-win32-x64", "x86_64-pc-windows-msvc")
        };
        for root in &roots {
            for parent in [
                root.join("@openai/codex/node_modules/@openai"),
                root.join("@openai"),
            ] {
                let path = parent
                    .join(package)
                    .join("vendor")
                    .join(triple)
                    .join("bin/codex.exe");
                if path.is_file() {
                    return Ok(json!([path]));
                }
            }
        }
    }
    if let Some(path) = executable(provider) {
        return Ok(json!([path]));
    }
    if provider == "claude" {
        if let Some(home) = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
            let path = PathBuf::from(home)
                .join(".local/bin")
                .join(if cfg!(windows) {
                    "claude.exe"
                } else {
                    "claude"
                });
            if path.is_file() {
                return Ok(json!([path]));
            }
        }
    }
    bail!("native executable not found for {provider}; supply --executables or install the provider separately")
}
pub fn discover(document: &Value) -> Result<Value> {
    let mut result = serde_json::Map::new();
    for spec in document["agents"]
        .as_object()
        .context("config agents must be a table")?
        .values()
    {
        let name = spec["provider"]
            .as_str()
            .context("agent provider missing")?;
        if !result.contains_key(name) {
            result.insert(name.into(), provider(name)?);
        }
    }
    Ok(Value::Object(result))
}
