//! Role identity, installed default-name lookup, and unambiguous local routing.
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::{
    io::Read,
    path::{Path, PathBuf},
};

pub fn normalize(value: &str) -> Result<String> {
    let value = value.trim().to_lowercase();
    ensure!(
        value.contains('.')
            && value.split('.').all(|part| !part.is_empty())
            && value
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c)),
        "role ID must use publisher.role form"
    );
    Ok(match value.as_str() {
        "ccb.archi" => "agentroles.archi".into(),
        "agentrole.ccb_self" => "agentroles.ccb_self".into(),
        _ => value,
    })
}
pub fn normalized(value: &Value) -> Result<Option<String>> {
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(normalize(
        value.as_str().context("agent role must be a string")?,
    )?))
}
pub fn resolve(target: &str, agents: &Value) -> Result<String> {
    let target = target.trim().to_lowercase();
    if target == "all" || agents[&target].is_object() {
        return Ok(target);
    }
    if !target.contains('.') {
        return Ok(target);
    }
    let identity = normalize(&target)?;
    let mut matches = Vec::new();
    for (name, spec) in agents.as_object().context("agents must be an object")? {
        if normalized(&spec["role"])?.as_deref() == Some(identity.as_str()) {
            matches.push(name.as_str());
        }
    }
    matches.sort_unstable();
    match matches.as_slice() {
        [one] => Ok((*one).into()),
        [] => bail!("role {identity} is not bound to any configured agent"),
        _ => bail!(
            "role {identity} is bound to multiple agents: {}; target an agent name explicitly",
            matches.join(", ")
        ),
    }
}
fn expand(path: &Path) -> Result<PathBuf> {
    if path.starts_with("~") {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .context("cannot expand role store home")?;
        return Ok(PathBuf::from(home).join(path.strip_prefix("~")?));
    }
    Ok(path.to_path_buf())
}
fn configured_path(key: &str) -> Result<Option<PathBuf>> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(|v| expand(Path::new(&v)))
        .transpose()
}
fn user_store() -> Result<PathBuf> {
    if let Some(root) = configured_path("AGENT_ROLES_STORE")? {
        return Ok(root.join("installed"));
    }
    if let Some(root) = configured_path("CCB_SOURCE_HOME")? {
        return Ok(root.join(".roles/installed"));
    }
    let managed = [
        "CCB_CALLER_ACTOR",
        "CCB_CALLER_RUNTIME_DIR",
        "CCB_SESSION_FILE",
        "CCB_SESSION_ID",
        "CCB_NATIVE_TOKEN_FILE",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()));
    let home = if !managed {
        configured_path("HOME")?.filter(|path| !managed_home(path))
    } else {
        None
    };
    let home = home
        .or(configured_path(if cfg!(windows) {
            "USERPROFILE"
        } else {
            "HOME"
        })?)
        .context("role lookup requires a source home or AGENT_ROLES_STORE")?;
    Ok(home.join(".roles/installed"))
}
fn managed_home(path: &Path) -> bool {
    let parts = path
        .components()
        .map(|p| p.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>();
    parts
        .windows(5)
        .any(|p| p[0] == "agents" && p[2] == "provider-state" && p[4] == "home")
}
fn toml(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "role manifest exceeds 1 MiB");
    let value: toml::Value =
        toml::from_str(std::str::from_utf8(&bytes).context("role manifest must be UTF-8")?)?;
    Ok(serde_json::to_value(value)?)
}
fn candidates(identity: &str) -> Vec<&str> {
    match identity {
        "agentroles.archi" => vec![identity, "ccb.archi"],
        "agentroles.ccb_self" => vec![identity, "agentrole.ccb_self"],
        _ => vec![identity],
    }
}
fn find(root: &Path, identity: &str) -> Result<Option<PathBuf>> {
    for id in candidates(identity) {
        let directory = root.join(id);
        let current = directory.join("current");
        if current.exists() {
            ensure!(current.is_dir(), "role current path must be a directory");
            return Ok(Some(current.canonicalize()?));
        }
        if directory.join("role.toml").is_file() {
            return Ok(Some(directory.canonicalize()?));
        }
    }
    Ok(None)
}
pub fn default_agent(identity: &str, project: Option<&Path>) -> Result<String> {
    let identity = normalize(identity)?;
    let mut found = None;
    if let Some(project) = project {
        for store in [
            project.join("roles/installed"),
            project.join(".ccb/roles/installed"),
        ] {
            if let Some(root) = find(&store, &identity)? {
                found = Some(root);
                break;
            }
        }
    }
    let root = match found {
        Some(root) => root,
        None => {
            find(&user_store()?, &identity)?.context(format!("role {identity} is not installed"))?
        }
    };
    let manifest = toml(&root.join("role.toml"))?;
    ensure!(
        normalize(manifest["id"].as_str().context("role manifest has no id")?)? == identity,
        "role manifest id mismatch"
    );
    ensure!(
        manifest["identity"].is_null() || manifest["identity"].is_object(),
        "role identity must be a table"
    );
    let mut name = None;
    for key in ["default_agent_name", "default_name"] {
        if !manifest["identity"][key].is_null() {
            let value = manifest["identity"][key]
                .as_str()
                .context("role default name must be a string")?
                .trim();
            if !value.is_empty() {
                name = Some(value.to_owned());
                break;
            }
        }
    }
    if name.is_none() {
        let path = root.join("adapters/ccb/adapter.toml");
        if path.is_file() {
            let adapter = toml(&path)?;
            if let Some(value) = adapter["default_agent_name"]
                .as_str()
                .filter(|v| !v.trim().is_empty())
            {
                name = Some(value.trim().to_owned());
            }
        }
    }
    let name = name
        .unwrap_or_else(|| identity.rsplit('.').next().unwrap().to_owned())
        .to_lowercase();
    crate::models::validate_agent(&name)?;
    Ok(name)
}
