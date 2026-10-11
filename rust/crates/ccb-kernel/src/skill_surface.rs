//! Recoverable, scoped Claude discovery links. Original skill trees are read-only.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

type Links = BTreeMap<String, PathBuf>;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema_version: u32,
    owner: String,
    owned: Links,
    desired: Links,
}
pub struct Routes {
    pub aliases: Vec<String>,
    pub removed: Vec<String>,
}
pub struct Surface {
    pub root: PathBuf,
    manifest: PathBuf,
    owner: String,
}
impl Surface {
    pub fn provision(store: &crate::store::Store, agent: &str) -> Result<Self> {
        let root = store.owned_path(&format!("providers/claude/{agent}/role-view"))?;
        let manifest = store.owned_path(&format!("providers/claude/{agent}/role-view.json"))?;
        let mut state = if manifest.exists() {
            read(&manifest)?
        } else {
            ensure!(!root.exists(), "unowned Claude role view already exists");
            let state = State {
                schema_version: 1,
                owner: uuid::Uuid::new_v4().to_string(),
                owned: Links::new(),
                desired: Links::new(),
            };
            fs::create_dir_all(manifest.parent().unwrap())?;
            save(&manifest, &state)?;
            state
        };
        let surface = Self {
            root,
            manifest,
            owner: state.owner.clone(),
        };
        surface.validate(&state)?;
        // A new owned process starts with an empty discovery view; its first
        // task rebinds the current role declarations before model input.
        state.desired.clear();
        save(&surface.manifest, &state)?;
        surface.reconcile(state)?;
        Ok(surface)
    }
    pub fn sync(&self, roots: &[PathBuf]) -> Result<Routes> {
        let mut state = read(&self.manifest)?;
        self.validate(&state)?;
        let previous = state.owned.clone();
        let mut desired = Links::new();
        for root in roots {
            plain_skill(root)?;
            desired.insert(alias(&self.owner, root), root.clone());
        }
        ensure!(desired.len() <= 64, "too many Claude role roots");
        state
            .desired
            .retain(|name, target| desired.get(name) == Some(target));
        save(&self.manifest, &state)?;
        state = self.reconcile(state)?;
        state.owned.extend(desired.clone());
        state.desired = desired;
        save(&self.manifest, &state)?; // Ownership intent precedes filesystem changes.
        let state = self.reconcile(state)?;
        Ok(Routes {
            aliases: state.desired.keys().cloned().collect(),
            removed: previous
                .keys()
                .filter(|name| !state.desired.contains_key(*name))
                .cloned()
                .collect(),
        })
    }
    fn validate(&self, state: &State) -> Result<()> {
        ensure!(
            state.schema_version == 1
                && state.owner == self.owner
                && uuid::Uuid::parse_str(&state.owner).is_ok(),
            "invalid Claude role view owner"
        );
        ensure!(
            state.owned.len() <= 128 && state.desired.len() <= 64,
            "invalid Claude role view capacity"
        );
        for (name, target) in &state.owned {
            ensure!(
                target.is_absolute() && *name == alias(&state.owner, target),
                "invalid owned skill alias"
            );
        }
        ensure!(
            state
                .desired
                .iter()
                .all(|(n, p)| state.owned.get(n) == Some(p)),
            "unowned desired skill alias"
        );
        Ok(())
    }
    fn reconcile(&self, mut state: State) -> Result<State> {
        self.validate(&state)?;
        for target in state.desired.values() {
            plain_skill(target)?;
        }
        let directory = self.root.join(".claude/skills");
        regular_dir(&self.root)?;
        regular_dir(&self.root.join(".claude"))?;
        regular_dir(&directory)?;
        only_entries(&self.root, &[".claude".into()])?;
        only_entries(&self.root.join(".claude"), &["skills".into()])?;
        let allowed: Vec<_> = state
            .owned
            .keys()
            .flat_map(|name| [name.clone(), format!(".{name}.pending")])
            .collect();
        only_entries(&directory, &allowed)?;
        for (name, target) in &state.owned {
            let entry = directory.join(name);
            let temp = directory.join(format!(".{name}.pending"));
            if fs::symlink_metadata(&temp).is_ok() {
                if crate::live_directory::is_link(&temp)? {
                    crate::live_directory::remove(&temp, target)?;
                } else {
                    ensure!(temp.is_dir(), "invalid pending role link");
                    fs::remove_dir(&temp)?;
                }
            }
            if fs::symlink_metadata(&entry).is_ok() {
                crate::live_directory::verify(&entry, target)?;
                if !state.desired.contains_key(name) {
                    crate::live_directory::remove(&entry, target)?;
                }
            } else if state.desired.contains_key(name) {
                crate::live_directory::create(&temp, target)?;
                crate::live_directory::publish(&temp, &entry)?;
                crate::live_directory::verify(&entry, target)?;
            }
        }
        state.owned = state.desired.clone();
        save(&self.manifest, &state)?;
        Ok(state)
    }
}
fn alias(owner: &str, target: &Path) -> String {
    let digest = Sha256::digest(format!("{owner}\n{}", target.display()).as_bytes());
    format!("ccb-role-{}", &format!("{digest:x}")[..20])
}
fn regular_dir(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        ensure!(
            !crate::live_directory::is_link(path)? && path.is_dir(),
            "linked/non-directory Claude role view"
        );
    } else {
        fs::create_dir(path)?;
    }
    Ok(())
}
fn only_entries(path: &Path, allowed: &[String]) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid role view entry"))?;
        ensure!(allowed.contains(&name), "unowned Claude role view entry");
    }
    Ok(())
}
fn plain_skill(root: &Path) -> Result<()> {
    // Native skill-directory plugins can activate components at discovery time.
    // Those require the separate role-hook/permission migration.
    for path in [
        ".claude-plugin/plugin.json",
        ".mcp.json",
        ".lsp.json",
        "hooks/hooks.json",
        "settings.json",
    ] {
        ensure!(
            !root.join(path).exists(),
            "role skill plugin components are not supported"
        );
    }
    Ok(())
}
fn read(path: &Path) -> Result<State> {
    ensure!(
        !crate::live_directory::is_link(path)?,
        "linked role view manifest"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "role view manifest exceeds limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn save(path: &Path, state: &State) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        ensure!(
            !crate::live_directory::is_link(path)?,
            "linked role view manifest"
        );
    }
    let parent = path.parent().context("missing role view parent")?;
    let temp = parent.join(format!(".role-view-{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(state)?)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(root: &Path) -> PathBuf {
        let path = root.join("original");
        fs::create_dir(&path).unwrap();
        fs::write(
            path.join("SKILL.md"),
            "---\nname: reader\ndescription: Read.\n---\nBody.",
        )
        .unwrap();
        fs::write(path.join("keep.txt"), "untouched").unwrap();
        path.canonicalize().unwrap()
    }
    #[test]
    fn live_reference_changes_and_removal_preserve_source() {
        let temp = tempfile::tempdir().unwrap();
        let target = source(temp.path());
        let store = crate::store::Store::open(&temp.path().join("state")).unwrap();
        let surface = Surface::provision(&store, "worker").unwrap();
        let routes = surface.sync(std::slice::from_ref(&target)).unwrap();
        let link = surface.root.join(".claude/skills").join(&routes.aliases[0]);
        assert_eq!(
            fs::read_to_string(link.join("keep.txt")).unwrap(),
            "untouched"
        );
        fs::write(target.join("keep.txt"), "live update").unwrap();
        assert_eq!(
            fs::read_to_string(link.join("keep.txt")).unwrap(),
            "live update"
        );
        assert_eq!(surface.sync(&[]).unwrap().removed, routes.aliases);
        assert!(fs::symlink_metadata(link).is_err());
        assert_eq!(
            fs::read_to_string(target.join("keep.txt")).unwrap(),
            "live update"
        );
    }
    #[test]
    fn journaled_pending_link_recovers_without_touching_source() {
        let temp = tempfile::tempdir().unwrap();
        let target = source(temp.path());
        let store = crate::store::Store::open(&temp.path().join("state")).unwrap();
        let surface = Surface::provision(&store, "worker").unwrap();
        let name = alias(&surface.owner, &target);
        let mut state = read(&surface.manifest).unwrap();
        state.owned.insert(name.clone(), target.clone());
        state.desired = state.owned.clone();
        save(&surface.manifest, &state).unwrap();
        let pending = surface
            .root
            .join(".claude/skills")
            .join(format!(".{name}.pending"));
        crate::live_directory::create(&pending, &target).unwrap();
        let restored = Surface::provision(&store, "worker").unwrap();
        assert!(fs::symlink_metadata(&pending).is_err());
        assert_eq!(
            restored
                .sync(std::slice::from_ref(&target))
                .unwrap()
                .aliases,
            vec![name]
        );
        assert_eq!(
            fs::read_to_string(target.join("keep.txt")).unwrap(),
            "untouched"
        );
    }
    #[test]
    fn substituted_directory_is_not_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let target = source(temp.path());
        let store = crate::store::Store::open(&temp.path().join("state")).unwrap();
        let surface = Surface::provision(&store, "worker").unwrap();
        let routes = surface.sync(std::slice::from_ref(&target)).unwrap();
        let path = surface.root.join(".claude/skills").join(&routes.aliases[0]);
        crate::live_directory::remove(&path, &target).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("user.txt"), "preserve").unwrap();
        assert!(surface.sync(&[]).is_err());
        assert_eq!(
            fs::read_to_string(path.join("user.txt")).unwrap(),
            "preserve"
        );
        assert_eq!(
            fs::read_to_string(target.join("keep.txt")).unwrap(),
            "untouched"
        );
    }
}
