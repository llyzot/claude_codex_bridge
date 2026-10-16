//! Project the CCB control skills (ask, ccb-clear, ccb-compact) into each
//! provider's user skill directory. Only directories this command installed
//! (recorded by a manifest of file hashes) are replaced; edited or foreign
//! skills are left alone unless explicitly replaced, and then backed up first.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

const MANIFEST: &str = ".ccb-skill.json";
const PROVIDERS: [&str; 3] = ["pi", "claude", "codex"];

macro_rules! skill {
    ($($path:literal),*) => { &[$(($path, include_str!(concat!("../../../../inherit_skills/", $path)))),*] };
}
/// (source path, contents) pairs per provider. On Windows a
/// `SKILL.md.powershell` variant is installed as `SKILL.md`, as the legacy
/// installer did.
fn sources(provider: &str) -> &'static [(&'static str, &'static str)] {
    match provider {
        "pi" => skill!(
            "pi_skills/ask/SKILL.md",
            "pi_skills/ccb-clear/SKILL.md",
            "pi_skills/ccb-compact/SKILL.md"
        ),
        "claude" => skill!(
            "claude_skills/ask/SKILL.md",
            "claude_skills/ask/SKILL.md.powershell",
            "claude_skills/ccb-clear/SKILL.md",
            "claude_skills/ccb-clear/agents/openai.yaml",
            "claude_skills/ccb-compact/SKILL.md",
            "claude_skills/ccb-compact/agents/openai.yaml"
        ),
        _ => skill!(
            "codex_skills/ask/SKILL.md",
            "codex_skills/ccb-clear/SKILL.md",
            "codex_skills/ccb-clear/agents/openai.yaml",
            "codex_skills/ccb-compact/SKILL.md",
            "codex_skills/ccb-compact/agents/openai.yaml"
        ),
    }
}

/// skill name -> (relative file -> contents) for this platform.
fn skills(provider: &str) -> Vec<(String, Vec<(String, &'static str)>)> {
    let mut out: Vec<(String, Vec<(String, &'static str)>)> = Vec::new();
    for (path, text) in sources(provider) {
        let mut parts = path.splitn(3, '/');
        let (_, name, rel) = (parts.next(), parts.next().unwrap(), parts.next().unwrap());
        let rel = match rel {
            "SKILL.md.powershell" if cfg!(windows) => "SKILL.md",
            "SKILL.md.powershell" => continue,
            "SKILL.md"
                if cfg!(windows)
                    && sources(provider).iter().any(|(p, _)| {
                        *p == format!("{}_skills/{name}/SKILL.md.powershell", provider)
                    }) =>
            {
                continue
            }
            other => other,
        };
        match out.iter_mut().find(|(n, _)| n == name) {
            Some((_, files)) => files.push((rel.into(), *text)),
            None => out.push((name.into(), vec![(rel.into(), *text)])),
        }
    }
    out
}

fn home() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("cannot determine the home directory")
}
/// The provider's user skill root, honoring its documented override.
pub(crate) fn root(provider: &str) -> Result<PathBuf> {
    let (var, default): (&str, &[&str]) = match provider {
        "pi" => ("PI_CODING_AGENT_DIR", &[".pi", "agent"]),
        "claude" => ("CLAUDE_CONFIG_DIR", &[".claude"]),
        "codex" => ("CODEX_HOME", &[".codex"]),
        _ => bail!("unsupported skill provider: {provider}"),
    };
    let base = match std::env::var_os(var).filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => default.iter().fold(home()?, |p, s| p.join(s)),
    };
    Ok(base.join("skills"))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn expected(files: &[(String, &str)]) -> Value {
    files
        .iter()
        .map(|(rel, text)| (rel.clone(), json!(digest(text.as_bytes()))))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// absent | current | outdated | modified | unmanaged
fn state(dir: &Path, files: &[(String, &str)]) -> Result<&'static str> {
    if !dir.exists() {
        return Ok("absent");
    }
    let Ok(bytes) = fs::read(dir.join(MANIFEST)) else {
        return Ok("unmanaged");
    };
    let manifest: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let Some(recorded) = manifest["files"]
        .as_object()
        .filter(|_| manifest["schema_version"] == 1)
    else {
        return Ok("unmanaged");
    };
    for (rel, hash) in recorded {
        match fs::read(dir.join(rel)) {
            Ok(bytes) if json!(digest(&bytes)) == *hash => {}
            _ => return Ok("modified"),
        }
    }
    Ok(if manifest["files"] == expected(files) {
        "current"
    } else {
        "outdated"
    })
}

fn write(dir: &Path, name: &str, provider: &str, files: &[(String, &str)]) -> Result<()> {
    let root = dir.parent().context("skill root")?;
    // Stage outside the scanned skill root so a partial copy is never loaded.
    let stage = root.parent().context("skill root parent")?.join(format!(
        ".ccb-skill-stage-{provider}-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    for (rel, text) in files {
        let path = stage.join(rel);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, text)?;
    }
    let manifest = json!({"schema_version":1,"record_type":"ccb_skill","name":name,"provider":provider,
        "version":crate::installation::version(),"files":expected(files)});
    fs::write(stage.join(MANIFEST), serde_json::to_vec_pretty(&manifest)?)?;
    fs::create_dir_all(root)?;
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::rename(&stage, dir)?;
    Ok(())
}

pub(crate) fn run(
    action: &str,
    providers: &[String],
    dry_run: bool,
    replace_unmanaged: bool,
) -> Result<Value> {
    ensure!(
        ["status", "install"].contains(&action),
        "skills requires status or install"
    );
    let selected: Vec<&str> = if providers.is_empty() {
        PROVIDERS.to_vec()
    } else {
        providers
            .iter()
            .map(|p| {
                PROVIDERS
                    .iter()
                    .copied()
                    .find(|k| k == p)
                    .context("unsupported skill provider")
            })
            .collect::<Result<_>>()?
    };
    let mut rows = Vec::new();
    for provider in selected {
        let root = root(provider)?;
        for (name, files) in skills(provider) {
            let dir = root.join(&name);
            let before = state(&dir, &files)?;
            let action_taken = match (action, before) {
                ("status", _) | (_, "current") => "none",
                (_, "absent" | "outdated") => "install",
                (_, "unmanaged") if replace_unmanaged => "backup_and_install",
                (_, "unmanaged") => "skipped_unmanaged",
                _ => "skipped_modified",
            };
            if action == "install" && !dry_run {
                if action_taken == "backup_and_install" {
                    let backups = root
                        .parent()
                        .context("skill root parent")?
                        .join(".ccb-skill-backups");
                    fs::create_dir_all(&backups)?;
                    let stamp = chrono_stamp();
                    fs::rename(&dir, backups.join(format!("{name}-{stamp}")))?;
                }
                if ["install", "backup_and_install"].contains(&action_taken) {
                    write(&dir, &name, provider, &files)?;
                }
            }
            rows.push(json!({"provider":provider,"skill":name,"path":dir,"state":before,"action":action_taken}));
        }
    }
    Ok(
        json!({"status":if action=="status" {"observed"} else if dry_run {"planned"} else {"applied"},
        "version":crate::installation::version(),"skills":rows}),
    )
}

fn chrono_stamp() -> String {
    crate::daemon::now().replace([':', '.'], "-")
}
