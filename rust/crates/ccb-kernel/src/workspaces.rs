//! Read-only workspace plans and recoverable Git materialization. Never resets,
//! prunes or removes a checkout, branch, or original project tree.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
};

const MARKER: &str = ".ccb-workspace.json";
pub(crate) fn portable(path: &Path) -> Result<String> {
    let text = path.to_str().context("workspace path must be UTF-8")?;
    #[cfg(windows)]
    {
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return Ok(format!(r"\\{unc}"));
        }
        Ok(text.strip_prefix(r"\\?\").unwrap_or(text).into())
    }
    #[cfg(not(windows))]
    {
        Ok(text.into())
    }
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    schema_version: u32,
    source_root: PathBuf,
    common_dir: PathBuf,
    project_id: String,
    agent_name: String,
    scope: String,
    group: Option<String>,
    pub workspace_path: PathBuf,
    branch: Option<String>,
    base_ref: String,
}
#[derive(Clone)]
struct Registered {
    path: PathBuf,
    branch: Option<String>,
}

fn run(root: &Path, args: &[&str], hooks: Option<&Path>) -> Result<Output> {
    let mut command = Command::new("git");
    command.arg("-C").arg(portable(root)?);
    if let Some(path) = hooks {
        command
            .arg("-c")
            .arg(format!("core.hooksPath={}", portable(path)?));
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(key);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().context("Git executable unavailable")?;
    ensure!(
        output.stdout.len() + output.stderr.len() <= 8 * 1024 * 1024,
        "Git workspace output exceeded limit"
    );
    Ok(output)
}
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = run(root, args, None)?;
    ensure!(
        output.status.success(),
        "Git workspace inspection failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?
        .trim_end_matches(['\r', '\n'])
        .into())
}
fn absolute(base: &Path, path: &Path) -> Result<PathBuf> {
    let path = crate::config::expand_home(path)?;
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(normalized.pop(), "workspace path escapes filesystem root");
            }
            part => normalized.push(part.as_os_str()),
        }
    }
    let mut parent = normalized.clone();
    let mut tail = Vec::new();
    while !parent.exists() {
        tail.push(
            parent
                .file_name()
                .context("workspace has no existing ancestor")?
                .to_owned(),
        );
        ensure!(parent.pop(), "workspace has no existing ancestor");
    }
    let mut path = parent.canonicalize()?;
    for part in tail.into_iter().rev() {
        path.push(part);
    }
    Ok(path)
}
fn unlinked(path: &Path) -> Result<()> {
    let mut prefix = PathBuf::new();
    for part in path.components() {
        prefix.push(part);
        if let Ok(meta) = fs::symlink_metadata(&prefix) {
            ensure!(
                !meta.file_type().is_symlink(),
                "linked workspace control path"
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    meta.file_attributes() & 0x400 == 0,
                    "reparse workspace control path"
                );
            }
        }
    }
    Ok(())
}
fn load(path: &Path) -> Result<Option<Value>> {
    unlinked(path)?;
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    (&mut file).take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "workspace metadata exceeds limit"
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}
fn save(path: &Path, value: &Value) -> Result<()> {
    unlinked(path)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path)?;
    Ok(())
}
fn publish_binding(path: &Path, value: &Value) -> Result<()> {
    unlinked(path)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = temp.as_os_str().encode_wide().chain([0]).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0)
        };
        if ok == 0 {
            let error = std::io::Error::last_os_error();
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
    }
    #[cfg(not(windows))]
    {
        fs::hard_link(&temp, path)?;
        fs::remove_file(&temp)?;
    }
    Ok(())
}
fn optional<'a>(raw: &'a Value, key: &str) -> Result<Option<&'a str>> {
    if raw[key].is_null() {
        return Ok(None);
    }
    Ok(Some(
        raw[key]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .context(format!("invalid workspace field: {key}"))?
            .trim(),
    ))
}
fn common(root: &Path) -> Result<PathBuf> {
    absolute(
        root,
        Path::new(&git(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?),
    )
}
fn registered(root: &Path) -> Result<Vec<Registered>> {
    let data = git(root, &["worktree", "list", "--porcelain", "-z"])?;
    let mut rows = Vec::<Registered>::new();
    for field in data.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            rows.push(Registered {
                path: absolute(root, Path::new(path))?,
                branch: None,
            });
        } else if let Some(branch) = field.strip_prefix("branch refs/heads/") {
            if let Some(row) = rows.last_mut() {
                row.branch = Some(branch.into());
            }
        }
    }
    Ok(rows)
}
fn revision(root: &Path, name: &str) -> Result<String> {
    git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{name}^{{commit}}"),
        ],
    )
}
fn branch_exists(root: &Path, name: &str) -> Result<bool> {
    let result = run(
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
        None,
    )?;
    ensure!(
        result.status.success() || result.status.code() == Some(1),
        "cannot inspect workspace branch"
    );
    Ok(result.status.success())
}
fn validate_branch(root: &Path, name: &str) -> Result<()> {
    ensure!(
        !name.starts_with('-') && !name.contains("@{"),
        "invalid workspace branch"
    );
    ensure!(
        git(root, &["check-ref-format", "--branch", name])? == name,
        "workspace branch must be a literal name"
    );
    Ok(())
}
fn slug(root: &Path, id: &str) -> Result<String> {
    if let Some(identity) = load(&root.join(".ccb/project.identity.json"))? {
        if let Some(slug) = identity["project_slug"].as_str() {
            ensure!(
                !slug.is_empty()
                    && slug != "."
                    && slug != ".."
                    && slug
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                "invalid project slug"
            );
            return Ok(slug.into());
        }
    }
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_lowercase();
    let mut clean = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || "._-".contains(ch) {
            clean.push(ch)
        } else if !clean.ends_with('-') {
            clean.push('-')
        }
    }
    let clean = clean.trim_matches('-');
    Ok(format!(
        "{}-{}",
        if clean.is_empty() { "project" } else { clean },
        &id[..8]
    ))
}
pub fn plan(root: &Path, name: &str, raw: &Value) -> Result<(PathBuf, Option<Plan>)> {
    let source = absolute(root, Path::new(optional(raw, "target")?.unwrap_or(".")))?;
    ensure!(source.is_dir(), "agent target directory does not exist");
    let mode = optional(raw, "workspace_mode")?.unwrap_or("inplace");
    let external = optional(raw, "workspace_path")?;
    let group = optional(raw, "workspace_group")?;
    let base = optional(raw, "workspace_root")?;
    let template = optional(raw, "branch_template")?;
    ensure!(
        [external.is_some(), group.is_some(), base.is_some()]
            .into_iter()
            .filter(|x| *x)
            .count()
            <= 1,
        "workspace overrides are mutually exclusive"
    );
    ensure!(
        (external.is_none() && group.is_none()) || (mode == "git-worktree" && template.is_none()),
        "external/group workspace requires git-worktree without branch_template"
    );
    if mode == "inplace" {
        return Ok((source, None));
    }
    ensure!(
        mode == "git-worktree",
        "native workspace mode not yet supported: {mode}"
    );
    let project_id = crate::config::project_id(&source)?;
    let common_dir = common(&source)?;
    let state = crate::config::state_root(&source)?
        .parent()
        .context("missing workspace state root")?
        .to_owned();
    let (path, scope, mut branch) = if let Some(path) = external {
        (absolute(&source, Path::new(path))?, "external", None)
    } else if let Some(group) = group {
        crate::models::validate_agent(group)?;
        (
            absolute(&source, &state.join("workspaces/groups").join(group))?,
            "group",
            Some(format!("ccb/group/{group}")),
        )
    } else {
        let path = if let Some(base) = base {
            absolute(&source, Path::new(base))?
                .join(slug(&source, &project_id)?)
                .join(name)
        } else {
            state.join("workspaces").join(name)
        };
        let date = crate::daemon::now()[..10].replace('-', "");
        let branch = template
            .unwrap_or("ccb/{agent_name}")
            .replace("{agent_name}", name)
            .replace("{project_slug}", &slug(&source, &project_id)?)
            .replace("{date}", &date);
        ensure!(
            !branch.contains(['{', '}']) && !branch.trim().is_empty(),
            "unsupported workspace branch template"
        );
        (
            absolute(&source, &path)?,
            "agent",
            Some(branch.trim().into()),
        )
    };
    if scope == "group" {
        if let Some(record) = load(&path.join(MARKER))? {
            branch = Some(
                record["branch_name"]
                    .as_str()
                    .context("group binding has no branch")?
                    .into(),
            );
        }
    }
    let plan = Plan {
        schema_version: 1,
        source_root: source,
        common_dir,
        project_id,
        agent_name: name.into(),
        scope: scope.into(),
        group: group.map(str::to_owned),
        workspace_path: path,
        branch,
        base_ref: optional(raw, "base_commit")?.unwrap_or("HEAD").into(),
    };
    plan.inspect(false)?;
    Ok((plan.workspace_path.clone(), Some(plan)))
}
impl Plan {
    fn sibling(&self, suffix: &str) -> PathBuf {
        self.workspace_path.with_file_name(format!(
            "{}.ccb-native-{suffix}",
            self.workspace_path.file_name().unwrap().to_string_lossy()
        ))
    }
    fn binding(&self) -> Result<Value> {
        Ok(
            json!({"schema_version":2,"record_type":"workspace_binding","target_project":portable(&self.source_root)?,"project_id":self.project_id,
        "agent_name":self.group.as_ref().unwrap_or(&self.agent_name),"workspace_mode":"git-worktree","workspace_path":portable(&self.workspace_path)?,"branch_name":self.branch}),
        )
    }
    fn check_binding(&self, value: &Value) -> Result<()> {
        ensure!(
            value["schema_version"] == 2
                && value["record_type"] == "workspace_binding"
                && value["workspace_mode"] == "git-worktree"
                && value["project_id"] == self.project_id
                && value["branch_name"]
                    == self
                        .branch
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
            "workspace binding identity mismatch"
        );
        ensure!(
            absolute(
                &self.source_root,
                Path::new(
                    value["target_project"]
                        .as_str()
                        .context("missing binding project")?
                )
            )? == self.source_root
                && absolute(
                    &self.source_root,
                    Path::new(
                        value["workspace_path"]
                            .as_str()
                            .context("missing binding path")?
                    )
                )? == self.workspace_path,
            "workspace binding path mismatch"
        );
        let owner = value["agent_name"]
            .as_str()
            .context("missing workspace binding owner")?;
        crate::models::validate_agent(owner)?;
        ensure!(
            self.scope == "group" || owner == self.agent_name,
            "workspace binding agent mismatch"
        );
        Ok(())
    }
    fn intent(&self) -> Result<Option<Value>> {
        let value = load(&self.sibling("worktree.json"))?;
        if let Some(value) = &value {
            ensure!(
                value["schema_version"] == 1
                    && value["record_type"] == "native_worktree_intent"
                    && value["binding"] == self.binding()?
                    && value["common_dir"] == json!(self.common_dir)
                    && ["prepared", "checkout_done", "bound"]
                        .contains(&value["phase"].as_str().unwrap_or("")),
                "workspace creation intent mismatch"
            );
        }
        Ok(value)
    }
    fn inspect(&self, materializing: bool) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && ["agent", "group", "external"].contains(&self.scope.as_str()),
            "invalid workspace plan"
        );
        crate::models::validate_agent(&self.agent_name)?;
        ensure!(
            (self.scope == "group") == self.group.is_some(),
            "invalid workspace group plan"
        );
        if let Some(group) = &self.group {
            crate::models::validate_agent(group)?;
        }
        ensure!(
            (self.scope == "external") == self.branch.is_none(),
            "invalid workspace branch plan"
        );
        ensure!(
            self.source_root.is_absolute()
                && self.workspace_path.is_absolute()
                && self.source_root != self.workspace_path,
            "invalid workspace path"
        );
        ensure!(
            crate::config::project_id(&self.source_root)? == self.project_id
                && common(&self.source_root)? == self.common_dir,
            "workspace source identity changed"
        );
        unlinked(&self.workspace_path)?;
        if let Some(branch) = &self.branch {
            validate_branch(&self.source_root, branch)?;
        }
        ensure!(
            self.scope == "external" || self.branch.is_some(),
            "managed workspace needs a branch"
        );
        let rows = registered(&self.source_root)?;
        let found = rows.iter().find(|r| r.path == self.workspace_path);
        if self.scope == "external" {
            ensure!(
                found.is_some(),
                "external workspace is not registered with the source repository"
            );
            self.existing()?;
            return Ok(());
        }
        let intent = self.intent()?;
        if let Some(binding) = load(&self.workspace_path.join(MARKER))? {
            self.check_binding(&binding)?;
            ensure!(found.is_some(), "bound workspace is not registered");
            self.existing()?;
            return Ok(());
        }
        if self.workspace_path.exists() || found.is_some() {
            let intent=intent.context("refusing to adopt an unbound workspace; use explicit workspace_path for an existing checkout")?;
            if materializing {
                ensure!(
                    found.is_some(),
                    "incomplete worktree creation requires manual repair"
                );
                self.existing()?;
                if intent["phase"] == "prepared" {
                    ensure!(
                        git(
                            &self.workspace_path,
                            &["status", "--porcelain=v1", "-z", "--untracked-files=all"]
                        )?
                        .is_empty()
                            && git(&self.workspace_path, &["rev-parse", "HEAD"])?
                                == intent["source_commit"],
                        "unconfirmed or changed worktree creation requires manual repair"
                    );
                }
            }
            return Ok(());
        }
        let branch = self.branch.as_deref().unwrap();
        ensure!(
            !rows.iter().any(|r| r.branch.as_deref() == Some(branch)),
            "workspace branch is already checked out elsewhere"
        );
        let selected = if branch_exists(&self.source_root, branch)? {
            format!("refs/heads/{branch}")
        } else {
            self.base_ref.clone()
        };
        let commit = revision(&self.source_root, &selected)?;
        ensure!(
            git(&self.source_root, &["ls-tree", "-z", &commit, "--", MARKER])?.is_empty(),
            "selected commit tracks reserved workspace binding"
        );
        Ok(())
    }
    fn existing(&self) -> Result<()> {
        ensure!(
            self.workspace_path.is_dir(),
            "registered workspace is missing"
        );
        ensure!(
            absolute(
                &self.workspace_path,
                Path::new(&git(
                    &self.workspace_path,
                    &["rev-parse", "--show-toplevel"]
                )?)
            )? == self.workspace_path
                && common(&self.workspace_path)? == self.common_dir,
            "workspace repository/root mismatch"
        );
        if let Some(branch) = &self.branch {
            ensure!(
                git(&self.workspace_path, &["branch", "--show-current"])? == *branch,
                "workspace branch mismatch"
            );
        }
        if self.scope != "external" {
            ensure!(
                git(&self.workspace_path, &["ls-files", "-z", "--", MARKER])?.is_empty(),
                "workspace binding is tracked; refusing to replace it"
            );
        }
        Ok(())
    }
    fn materialize(&self) -> Result<()> {
        if self.scope == "external" {
            return self.inspect(true);
        }
        unlinked(&self.workspace_path)?;
        let parent = self
            .workspace_path
            .parent()
            .context("missing workspace parent")?;
        fs::create_dir_all(parent)?;
        unlinked(parent)?;
        let lock_path = self.sibling("worktree.lock");
        unlinked(&lock_path)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        ensure!(
            lock.try_lock().is_ok(),
            "workspace materialization already in progress"
        );
        self.inspect(true)?;
        if let Some(binding) = load(&self.workspace_path.join(MARKER))? {
            self.check_binding(&binding)?;
            return Ok(());
        }
        let path = self.sibling("worktree.json");
        let mut intent = self.intent()?;
        if !self.workspace_path.exists() {
            let branch = self.branch.as_deref().unwrap();
            let exists = branch_exists(&self.source_root, branch)?;
            let selected = if exists {
                format!("refs/heads/{branch}")
            } else {
                self.base_ref.clone()
            };
            let commit = revision(&self.source_root, &selected)?;
            let record = json!({"schema_version":1,"record_type":"native_worktree_intent","binding":self.binding()?,"common_dir":self.common_dir,"source_commit":commit,"phase":"prepared"});
            save(&path, &record)?;
            intent = Some(record);
            let target = portable(&self.workspace_path)?;
            let args = if exists {
                vec!["worktree", "add", "--quiet", "--", &target, branch]
            } else {
                vec![
                    "worktree", "add", "--quiet", "-b", branch, "--", &target, &commit,
                ]
            };
            // A regular ownership file cannot contain an executable checkout hook.
            let result = run(&self.source_root, &args, Some(&path))?;
            ensure!(
                result.status.success(),
                "Git worktree creation failed; existing files were preserved: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            );
        }
        self.existing()?;
        let mut intent = intent.context("workspace creation has no intent")?;
        intent["phase"] = json!("checkout_done");
        save(&path, &intent)?;
        ensure!(
            !self.workspace_path.join(MARKER).exists(),
            "workspace binding appeared during creation"
        );
        publish_binding(&self.workspace_path.join(MARKER), &self.binding()?)?;
        intent["phase"] = json!("bound");
        save(&path, &intent)?;
        Ok(())
    }
}
fn plans(config: &Value) -> Result<Vec<Plan>> {
    let mut unique = BTreeMap::<PathBuf, Plan>::new();
    for (name, spec) in config["agents"].as_object().context("missing agents")? {
        if spec["workspace_plan"].is_null() {
            ensure!(
                spec["workspace_mode"] != "git-worktree",
                "git-worktree requires a compiled workspace plan"
            );
            continue;
        }
        let plan: Plan = serde_json::from_value(spec["workspace_plan"].clone())?;
        ensure!(
            plan.agent_name == *name && spec["workspace_path"] == json!(plan.workspace_path),
            "workspace plan/agent mismatch"
        );
        if let Some(prior) = unique.get(&plan.workspace_path) {
            ensure!(
                plan.scope == "group"
                    && prior.scope == "group"
                    && plan.group == prior.group
                    && plan.branch == prior.branch
                    && plan.source_root == prior.source_root
                    && plan.project_id == prior.project_id
                    && plan.common_dir == prior.common_dir
                    && plan.base_ref == prior.base_ref,
                "conflicting workspace plans"
            );
        } else {
            unique.insert(plan.workspace_path.clone(), plan);
        }
    }
    Ok(unique.into_values().collect())
}
pub fn validate(config: &Value) -> Result<()> {
    plans(config)?;
    Ok(())
}
pub fn materialize(config: &Value) -> Result<()> {
    let plans = plans(config)?;
    for plan in &plans {
        plan.inspect(false)?;
    }
    for plan in plans {
        plan.materialize()?;
    }
    Ok(())
}
pub fn binding_project(path: &Path) -> Result<Option<PathBuf>> {
    let Some(record) = load(&path.join(MARKER))? else {
        return Ok(None);
    };
    ensure!(
        record["schema_version"] == 2
            && record["record_type"] == "workspace_binding"
            && record["workspace_mode"] == "git-worktree",
        "invalid workspace binding"
    );
    let target = PathBuf::from(
        record["target_project"]
            .as_str()
            .context("missing workspace project")?,
    );
    ensure!(
        target.is_absolute(),
        "workspace binding project must be absolute"
    );
    let target = absolute(path, &target)?;
    ensure!(
        path.join(".git").is_file() && git(path, &["ls-files", "-z", "--", MARKER])?.is_empty(),
        "workspace discovery requires an untracked binding in a linked worktree"
    );
    ensure!(
        target != path
            && target.is_absolute()
            && target.is_dir()
            && absolute(
                path,
                Path::new(
                    record["workspace_path"]
                        .as_str()
                        .context("missing workspace path")?
                )
            )? == path
            && record["project_id"] == crate::config::project_id(&target)?
            && common(path)? == common(&target)?,
        "workspace binding source mismatch"
    );
    ensure!(
        registered(&target)?.iter().any(|r| r.path == path),
        "workspace binding is not registered"
    );
    Ok(Some(target))
}
