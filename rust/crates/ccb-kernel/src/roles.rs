//! Role identity, local routing and bounded per-turn reads of live role memory.
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
fn installed(identity: &str, project: Option<&Path>) -> Result<(PathBuf, Value)> {
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
    Ok((root, manifest))
}
pub fn default_agent(identity: &str, project: Option<&Path>) -> Result<String> {
    let identity = normalize(identity)?;
    let (root, manifest) = installed(&identity, project)?;
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

/// Read only the declared live files. The caller owns the per-turn audit commit;
/// content is returned for transport, never written into the job store here.
pub struct Prepared {
    pub text: String,
    pub audit: Value,
    pub skill_roots: Option<Vec<PathBuf>>,
}
pub fn prompt(spec: &Value, project: Option<&Path>, body: &str) -> Result<Prepared> {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    let Some(identity) = normalized(&spec["role"])? else {
        return Ok(Prepared {
            text: body.into(),
            audit: Value::Null,
            skill_roots: None,
        });
    };
    let (root, manifest) = installed(&identity, project)?;
    let schema = manifest["schema"].as_str().unwrap_or("");
    ensure!(
        manifest["schema"].is_null() || manifest["schema"].is_string(),
        "role schema must be a string"
    );
    let mut declarations = Vec::new();
    let mut adapter = Value::Null;
    let strategy = if schema.starts_with("agent-role/preview-") {
        ensure!(
            manifest["contents"].is_null() || manifest["contents"].is_object(),
            "role contents must be a table"
        );
        let path = root.join("adapters/ccb/adapter.toml");
        adapter = if path.exists() {
            ensure!(
                path.canonicalize()?.starts_with(&root),
                "role adapter escapes role root"
            );
            toml(&path)?
        } else {
            Value::Null
        };
        paths(&manifest["contents"]["memory"], &mut declarations)?;
        paths(&adapter["memory"], &mut declarations)?;
        adapter["memory_merge_strategy"].clone()
    } else {
        ensure!(
            schema.is_empty() || schema == "rolepack/v1",
            "unsupported role schema"
        );
        ensure!(
            manifest["memory"].is_null() || manifest["memory"].is_object(),
            "role memory must be a table"
        );
        paths(&manifest["memory"]["files"], &mut declarations)?;
        manifest["memory"]["merge_strategy"].clone()
    };
    let skill_roots = if ["codex", "pi"].contains(&spec["provider"].as_str().unwrap_or("")) {
        Some(skill_roots(
            &root,
            &manifest,
            &adapter,
            spec["provider"].as_str().unwrap(),
        )?)
    } else {
        None
    };
    ensure!(
        strategy.is_null() || strategy.as_str() == Some("append_after_project_memory"),
        "unsupported role memory merge strategy"
    );
    ensure!(declarations.len() <= 64, "too many role memory files");
    let mut sources = Vec::new();
    let mut text = String::new();
    let mut remaining = 256 * 1024;
    for relative in declarations {
        let path = root
            .join(&relative)
            .canonicalize()
            .context("role memory file is missing")?;
        ensure!(
            path.starts_with(&root) && path.is_file(),
            "role memory must be a file inside its role root"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?
            .take(remaining as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= remaining,
            "role memory exceeds 256 KiB per turn"
        );
        remaining -= bytes.len();
        let content = std::str::from_utf8(&bytes).context("role memory must be UTF-8")?;
        sources.push(json!({"path":relative,"sha256":format!("{:x}",Sha256::digest(&bytes)),"bytes":bytes.len()}));
        text.push_str(&format!("\n### {relative}\n{content}\n"));
    }
    let prompt = format!("{body}\n\n<ccb_role_context>\nCurrent role: {identity}\nRole root: {}\nThis is the current CCB role memory for this turn, replacing earlier CCB role memory. An empty list means no current role memory. Project and user instructions still apply.\n{text}</ccb_role_context>", root.display());
    Ok(Prepared {
        text: prompt,
        audit: json!({"role":identity,"root":root,"sources":sources,"skill_roots":skill_roots,"delivery":"per_turn_live_read"}),
        skill_roots,
    })
}

fn skill_roots(
    root: &Path,
    manifest: &Value,
    adapter: &Value,
    provider: &str,
) -> Result<Vec<PathBuf>> {
    let mut declarations = Vec::new();
    if manifest["schema"]
        .as_str()
        .unwrap_or("")
        .starts_with("agent-role/preview-")
    {
        let supported = if let Some(providers) = adapter["supported_providers"].as_array() {
            ensure!(
                providers.iter().all(Value::is_string),
                "invalid role skill providers"
            );
            if providers.is_empty() {
                None
            } else {
                Some(
                    providers
                        .iter()
                        .any(|v| v.as_str().unwrap().trim().eq_ignore_ascii_case(provider)),
                )
            }
        } else {
            ensure!(
                adapter["supported_providers"].is_null(),
                "role skill providers must be an array"
            );
            None
        };
        let supported = supported.unwrap_or_else(|| {
            adapter["recommended_provider"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().eq_ignore_ascii_case(provider))
                .unwrap_or(["codex", "claude"].contains(&provider))
        });
        if supported {
            paths(&manifest["contents"]["skills"], &mut declarations)?;
            paths(&adapter["skills"], &mut declarations)?;
        }
    } else {
        ensure!(
            manifest["skills"].is_null() || manifest["skills"].is_object(),
            "role skills must be a table"
        );
        paths(&manifest["skills"][provider], &mut declarations)?;
    }
    ensure!(declarations.len() <= 64, "too many role skills");
    let mut roots = Vec::new();
    for declaration in declarations {
        let path = root
            .join(declaration)
            .canonicalize()
            .context("role skill directory is missing")?;
        ensure!(
            path.starts_with(root) && path.is_dir(),
            "role skill directory escapes role root"
        );
        let file = path
            .join("SKILL.md")
            .canonicalize()
            .context("role skill has no SKILL.md")?;
        ensure!(
            file.starts_with(&path) && file.is_file(),
            "role SKILL.md escapes skill directory"
        );
        if !roots.contains(&path) {
            roots.push(path);
        }
    }
    Ok(roots)
}

fn paths(value: &Value, result: &mut Vec<String>) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    for item in value
        .as_array()
        .context("role memory paths must be an array")?
    {
        let path = item.as_str().context("role memory path must be a string")?;
        ensure!(
            !path.is_empty()
                && !path.contains([':', '\\'])
                && Path::new(path)
                    .components()
                    .all(|p| matches!(p, std::path::Component::Normal(_))),
            "role memory path must stay relative to its role root"
        );
        result.push(path.into());
    }
    Ok(())
}
