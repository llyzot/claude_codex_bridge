//! Live CCB shared/private files, read for the exact agent at turn dispatch.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

pub struct Prepared {
    pub shared: String,
    pub private: String,
    pub audit: Value,
}

fn source(
    root: &Path,
    relative: &str,
    kind: &str,
    remaining: &mut usize,
) -> Result<(String, Value)> {
    let mut path = root.to_owned();
    for part in Path::new(relative) {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "linked project memory path"
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        metadata.file_attributes() & 0x400 == 0,
                        "reparse project memory path"
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((
                    String::new(),
                    json!({"kind":kind,"path":relative,"exists":false,"bytes":0,"sha256":null}),
                ));
            }
            Err(e) => return Err(e.into()),
        }
    }
    let file = fs::File::open(&path)?;
    ensure!(
        file.metadata()?.is_file(),
        "project memory must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(*remaining as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= *remaining,
        "project memory exceeds 256 KiB per turn"
    );
    *remaining -= bytes.len();
    let text = std::str::from_utf8(&bytes)
        .context("project memory must be UTF-8")?
        .to_owned();
    Ok((
        text,
        json!({"kind":kind,"path":relative,"exists":true,"bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}),
    ))
}

pub fn prepare(project: Option<&Path>, agent: &str) -> Result<Option<Prepared>> {
    let Some(project) = project else {
        return Ok(None);
    };
    crate::models::validate_agent(agent)?;
    let root = project
        .canonicalize()
        .context("project memory root unavailable")?;
    ensure!(root.is_dir(), "project memory root must be a directory");
    let mut remaining = 256 * 1024;
    let (shared, shared_audit) = source(&root, ".ccb/ccb_memory.md", "ccb_shared", &mut remaining)?;
    let (private, private_audit) = source(
        &root,
        &format!(".ccb/agents/{agent}/memory.md"),
        "agent_private",
        &mut remaining,
    )?;
    let root_label = crate::workspaces::portable(&root)?;
    #[cfg(windows)]
    let root_label = root_label.replace('\\', "/");
    fn section(root: &str, tag: &str, title: &str, text: &str, audit: &Value) -> String {
        format!("\n\n<{tag}>\n{title}\nsource: {root}/{}\nCurrent CCB memory for this turn replaces earlier CCB memory from this source. Empty content means no current memory from this source. Project and user instructions still apply.\n{}\n</{tag}>",audit["path"].as_str().unwrap(),text.trim_end())
    }
    Ok(Some(Prepared {
        shared: section(
            &root_label,
            "ccb_shared_project_memory",
            "CCB Shared Project Memory",
            &shared,
            &shared_audit,
        ),
        private: section(
            &root_label,
            "ccb_agent_private_memory",
            "Agent Private Memory",
            &private,
            &private_audit,
        ),
        audit: json!({"agent_name":agent,"project_root":root,"delivery":"per_turn_live_read",
            "sources":[shared_audit,private_audit],"bytes":256*1024-remaining}),
    }))
}
