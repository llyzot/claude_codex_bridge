//! Git content identities without changing the real index, refs or object store.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct Git {
    root: PathBuf,
    scratch: tempfile::TempDir,
    objects: PathBuf,
}
fn portable(path: &Path) -> Result<String> {
    let text = path.to_str().context("Git workspace path must be UTF-8")?;
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
impl Git {
    fn run(&self, args: &[&str], private: bool, input: Option<&Path>) -> Result<Vec<u8>> {
        let mut command = Command::new("git");
        command.args([
            "-c",
            "core.splitIndex=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.fsmonitor=false",
        ]);
        command
            .arg("-C")
            .arg(portable(&self.root)?)
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                portable(&self.scratch.path().join("no-hooks"))?
            ));
        command
            .args(args)
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
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
        ] {
            command.env_remove(key);
        }
        command.env("GIT_LITERAL_PATHSPECS", "1");
        if private {
            command
                .env(
                    "GIT_INDEX_FILE",
                    portable(&self.scratch.path().join("index"))?,
                )
                .env(
                    "GIT_OBJECT_DIRECTORY",
                    portable(&self.scratch.path().join("objects"))?,
                )
                .env(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    serde_json::to_string(&portable(&self.objects)?)?,
                );
        }
        command.stdin(if let Some(path) = input {
            Stdio::from(fs::File::open(path)?)
        } else {
            Stdio::null()
        });
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let result = command
            .output()
            .context("Git observation executable unavailable")?;
        ensure!(
            result.stdout.len() + result.stderr.len() <= 64 * 1024 * 1024,
            "Git observation output exceeds limit"
        );
        ensure!(
            result.status.success(),
            "Git workspace observation failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
        Ok(result.stdout)
    }
    fn text(&self, args: &[&str], private: bool) -> Result<String> {
        let raw = self.run(args, private, None)?;
        let text = String::from_utf8(raw)?;
        Ok(text
            .strip_suffix('\n')
            .unwrap_or(&text)
            .trim_end_matches('\r')
            .to_owned())
    }
    fn path(&self, args: &[&str]) -> Result<PathBuf> {
        let value = PathBuf::from(self.text(args, false)?);
        Ok(if value.is_absolute() {
            value
        } else {
            self.root.join(value)
        })
    }
    fn index_entries(&self, private: bool) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let raw = self.run(&["ls-files", "--stage", "-z"], private, None)?;
        let mut result = BTreeMap::new();
        for row in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let tab = row
                .iter()
                .position(|b| *b == b'\t')
                .context("invalid Git index entry")?;
            let header = &row[..tab];
            ensure!(
                header.ends_with(b" 0"),
                "unmerged index cannot be bound to a review tree"
            );
            result.insert(row[tab + 1..].to_vec(), row.to_vec());
        }
        Ok(result)
    }
    fn add(&self, paths: Vec<Vec<u8>>, tracked: bool) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let file = self.scratch.path().join("paths");
        let mut bytes = Vec::new();
        for path in paths {
            bytes.extend(path);
            bytes.push(0);
        }
        fs::write(&file, bytes)?;
        let pathspec = format!("--pathspec-from-file={}", portable(&file)?);
        let mut args = vec!["add"];
        if tracked {
            args.push("-u");
        }
        args.extend([&pathspec, "--pathspec-file-nul"]);
        self.run(&args, true, None)?;
        Ok(())
    }
    fn tree(
        &self,
        head: &str,
        real: &BTreeMap<Vec<u8>, Vec<u8>>,
        prefix: &[u8],
        ignore: bool,
    ) -> Result<String> {
        self.run(&["read-tree", head], true, None)?;
        let baseline = self.index_entries(true)?;
        let mut entries = Vec::new();
        let zeros = "0".repeat(head.len());
        for path in baseline
            .keys()
            .filter(|path| selected(path, prefix, ignore))
        {
            entries.extend(format!("0 {zeros}\t").as_bytes());
            entries.extend(path);
            entries.push(0);
        }
        for (path, row) in real
            .iter()
            .filter(|(path, _)| selected(path, prefix, ignore))
        {
            let _ = path;
            entries.extend(row);
            entries.push(0);
        }
        if !entries.is_empty() {
            let file = self.scratch.path().join("entries");
            fs::write(&file, entries)?;
            self.run(&["update-index", "-z", "--index-info"], true, Some(&file))?;
        }
        let tracked = self
            .index_entries(true)?
            .into_keys()
            .filter(|p| selected(p, prefix, ignore))
            .collect();
        self.add(tracked, true)?;
        let untracked = self.run(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            true,
            None,
        )?;
        self.add(
            untracked
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty() && selected(p, prefix, ignore))
                .map(|p| p.to_vec())
                .collect(),
            false,
        )?;
        self.text(&["write-tree"], true)
    }
    fn check_submodules(&self, prefix: &[u8], ignore: bool) -> Result<()> {
        let raw = self.run(
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--ignore-submodules=none",
                "--untracked-files=normal",
            ],
            false,
            None,
        )?;
        let mut records = raw.split(|b| *b == 0);
        while let Some(row) = records.next() {
            let count = if row.starts_with(b"1 ") {
                9
            } else if row.starts_with(b"2 ") {
                records.next(); // The next NUL field is the rename's old path.
                10
            } else {
                continue;
            };
            let fields: Vec<_> = row.splitn(count, |b| *b == b' ').collect();
            if fields.len() == count && selected(fields[count - 1], prefix, ignore) {
                let sub = fields[2];
                ensure!(
                    sub.len() != 4 || sub[0] != b'S' || (sub[2] == b'.' && sub[3] == b'.'),
                    "dirty submodule content cannot be represented by a review tree"
                );
            }
        }
        Ok(())
    }
}
fn selected(path: &[u8], prefix: &[u8], ignore: bool) -> bool {
    let Some(relative) = path.strip_prefix(prefix) else {
        return false;
    };
    !ignore || !ignored(relative)
}
fn ignored(path: &[u8]) -> bool {
    let mut path = path;
    while let Some(rest) = path.strip_prefix(b"./") {
        path = rest;
    }
    [
        b".ccb".as_slice(),
        b"docs/plantree",
        b".ccb-workspace.json",
        b".mypy_cache",
        b".pytest_cache",
        b".ruff_cache",
        b"__pycache__",
    ]
    .iter()
    .any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(*prefix)
                .is_some_and(|tail| tail.starts_with(b"/"))
    }) || path
        .windows(b"/__pycache__/".len())
        .any(|s| s == b"/__pycache__/")
        || path.ends_with(b".pyc")
        || path.ends_with(b".pyo")
}
fn index_hash(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(format!("{:x}", Sha256::digest(bytes)))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn current(workspace: &Path, ignore: bool) -> Result<Value> {
    let workspace = workspace.canonicalize()?;
    let scratch = tempfile::Builder::new()
        .prefix("ccb-git-observation-")
        .tempdir()?;
    fs::write(scratch.path().join("no-hooks"), b"")?;
    fs::create_dir(scratch.path().join("objects"))?;
    let mut git = Git {
        root: workspace.clone(),
        scratch,
        objects: PathBuf::new(),
    };
    let root = git
        .path(&["rev-parse", "--show-toplevel"])?
        .canonicalize()?;
    ensure!(
        !git.scratch.path().canonicalize()?.starts_with(&root),
        "Git observation temporary directory must be outside the repository"
    );
    let mut prefix = workspace
        .strip_prefix(&root)?
        .to_string_lossy()
        .replace('\\', "/")
        .into_bytes();
    if !prefix.is_empty() {
        prefix.push(b'/');
    }
    git.root = root;
    git.objects = git
        .path(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?
        .canonicalize()?
        .join("objects");
    let format = git.text(&["rev-parse", "--show-object-format"], false)?;
    ensure!(
        ["sha1", "sha256"].contains(&format.as_str()),
        "unsupported Git object format"
    );
    let head = git.text(&["rev-parse", "--verify", "HEAD^{commit}"], false)?;
    let index = git.path(&["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let before = index_hash(&index)?;
    let real = git.index_entries(false)?;
    let flags = git.run(&["ls-files", "-v", "-z"], false, None)?;
    ensure!(
        !flags.split(|b| *b == 0).any(|row| row.len() > 2
            && b"Ss".contains(&row[0])
            && selected(&row[2..], &prefix, ignore)),
        "sparse or skip-worktree entries require explicit recovery before tree binding"
    );
    git.check_submodules(&prefix, ignore)?;
    let tree = git.tree(&head, &real, &prefix, ignore)?;
    ensure!(
        tree == git.tree(&head, &real, &prefix, ignore)?,
        "workspace changed while binding review tree"
    );
    git.check_submodules(&prefix, ignore)?;
    ensure!(
        before == index_hash(&index)?
            && head == git.text(&["rev-parse", "--verify", "HEAD^{commit}"], false)?,
        "Git HEAD or index changed during observation"
    );
    Ok(
        json!({"schema_version":1,"record_type":"git_workspace_tree","workspace_path":portable(&workspace)?,"repository_root":portable(&git.root)?,
        "head":head,"object_format":format,"digest":format!("git-tree:{format}:{tree}"),"ignore_controller_state":ignore}),
    )
}
