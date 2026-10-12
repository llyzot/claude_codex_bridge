//! Offline Git lifecycle transactions. No reset, force removal or silent
//! adoption; each external effect is preceded by an identity-bound intent.
use super::*;
use crate::{records, Kernel};

const POINTER: &str = "lifecycle.json";

pub(super) fn startup_guard(plan: &Plan) -> Result<()> {
    if let Some(pointer) = load(&plan.sibling(POINTER))? {
        ensure!(
            pointer["schema_version"] == 1
                && pointer["record_type"] == "native_worktree_lifecycle_binding"
                && pointer["binding"] == plan.binding()?,
            "worktree lifecycle binding changed"
        );
        ensure!(pointer["state"] == "available",
            "worktree has a pending lifecycle operation or is retired; recover/restore it or remove its agent configuration");
    }
    Ok(())
}
fn pointer(plan: &Plan, row: &Value, state: &str) -> Result<()> {
    save(
        &plan.sibling(POINTER),
        &json!({"schema_version":1,"record_type":"native_worktree_lifecycle_binding",
        "binding":plan.binding()?,"operation_id":row["operation_id"],"state":state}),
    )
}
fn op_root(runtime: &Path) -> Result<PathBuf> {
    Ok(runtime
        .parent()
        .context("missing runtime parent")?
        .join("worktree-operations"))
}
fn id(value: &str) -> Result<String> {
    let parsed = uuid::Uuid::parse_str(value)?;
    ensure!(
        parsed.to_string() == value,
        "operation ID must be canonical UUID"
    );
    Ok(value.into())
}
fn config(runtime: &Path) -> Result<Value> {
    let committed = load(&runtime.join("configuration.json"))?
        .context("worktree lifecycle requires a committed native configuration")?;
    crate::restart_config::restore(&committed)
}
fn selected(config: &Value, agent: &str) -> Result<Plan> {
    let name = crate::roles::resolve(agent, &config["agents"])?;
    let spec = &config["agents"][name];
    ensure!(
        spec["workspace_plan"].is_object(),
        "agent does not use a managed Git worktree"
    );
    let plan: Plan = serde_json::from_value(spec["workspace_plan"].clone())?;
    ensure!(
        plan.scope != "external",
        "external checkouts cannot be managed by lifecycle commands"
    );
    Ok(plan)
}
fn operation(runtime: &Path, operation: &str) -> Result<(PathBuf, Value, Plan)> {
    let path = op_root(runtime)?
        .join(id(operation)?)
        .join("operation.json");
    let row = load(&path)?.context("worktree operation not found")?;
    ensure!(
        row["schema_version"] == 1
            && row["record_type"] == "native_worktree_operation"
            && row["operation_id"] == operation
            && row["runtime_root"] == json!(runtime),
        "worktree operation identity changed"
    );
    let plan: Plan = serde_json::from_value(row["plan"].clone())?;
    ensure!(
        row["binding"] == plan.binding()?
            && row["project_id"] == plan.project_id
            && row["common_dir"] == json!(plan.common_dir),
        "worktree operation binding changed"
    );
    let expected = path.parent().unwrap();
    ensure!(
        row["stage_path"] == json!(expected.join("integration")),
        "worktree operation stage escaped its directory"
    );
    let action = records::string(&row, "action")?;
    ensure!(
        ["merge", "retire", "restore", "purge"].contains(&action),
        "invalid recorded worktree action"
    );
    if action != "merge" {
        let retirement = if action == "retire" {
            operation
        } else {
            records::string(&row, "retirement_id")?
        };
        id(retirement)?;
        ensure!(
            row["archive_path"] == json!(archive_path(&plan, retirement)?),
            "archive path escaped its retirement binding"
        );
        if action != "retire" {
            let origin = load(&op_root(runtime)?.join(retirement).join("operation.json"))?
                .context("retirement receipt missing")?;
            ensure!(
                origin["action"] == "retire"
                    && origin["status"] == "completed"
                    && origin["operation_id"] == retirement
                    && origin["binding"] == row["binding"]
                    && origin["plan"] == row["plan"]
                    && origin["source_commit"] == row["source_commit"]
                    && origin["archive_path"] == row["archive_path"],
                "retirement lineage changed"
            );
        }
    }
    Ok((path, row, plan))
}
fn archive_path(plan: &Plan, operation: &str) -> Result<PathBuf> {
    Ok(plan.workspace_path.with_file_name(format!(
        "{}.ccb-retired-{operation}",
        plan.workspace_path
            .file_name()
            .context("missing worktree name")?
            .to_string_lossy()
    )))
}
fn claim(plan: &Plan, row: &Value) -> Result<()> {
    let current = load(&plan.sibling(POINTER))?.unwrap_or(Value::Null);
    if current["operation_id"] == row["operation_id"] {
        ensure!(
            current["binding"] == row["binding"],
            "lifecycle pointer binding changed"
        );
    } else {
        ensure!(
            current == row["previous_pointer"] && row["phase"] == "prepared",
            "another operation owns this worktree"
        );
        pointer(plan, row, "pending")?;
    }
    Ok(())
}
fn finish_pointer(plan: &Plan, row: &Value) -> Result<()> {
    let current = load(&plan.sibling(POINTER))?.unwrap_or(Value::Null);
    // Re-reading an old receipt must never replace a later transaction's pointer.
    if current["operation_id"] == row["operation_id"] {
        ensure!(
            current["binding"] == row["binding"],
            "lifecycle pointer binding changed"
        );
        pointer(
            plan,
            row,
            if ["retire", "purge"].contains(&records::string(row, "action")?) {
                "retired"
            } else {
                "available"
            },
        )?;
    }
    Ok(())
}
fn pin(plan: &Plan, row: &Value, receipt: &Path) -> Result<()> {
    let retirement = if row["action"] == "retire" {
        records::string(row, "operation_id")?
    } else {
        records::string(row, "retirement_id")?
    };
    let reference = format!("refs/ccb/archive/{retirement}");
    let saved = records::string(row, "source_commit")?;
    let output = run(
        &plan.source_root,
        &["rev-parse", "--verify", &reference],
        None,
    )?;
    if output.status.success() {
        ensure!(
            String::from_utf8_lossy(&output.stdout).trim() == saved,
            "archive commit pin changed"
        );
    } else {
        ensure!(row["action"] == "retire", "archive commit pin missing");
        let zero = "0".repeat(saved.len());
        effect(
            &plan.source_root,
            &["update-ref", &reference, saved, &zero],
            receipt,
        )?;
    }
    Ok(())
}
fn hosted_stopped(row: &Value) -> Result<()> {
    // Closing a hosted pane alone does not prove that its app-server has ended.
    for key in ["host_process", "server_process"] {
        let process = &row["host_launch"][key];
        if !process.is_null() {
            ensure!(
                process.is_object()
                    && !crate::startup::owner_alive(
                        &json!({"pid":process["pid"],"process":process})
                    )?,
                "close the worktree's hosted provider process before lifecycle changes"
            );
        }
    }
    Ok(())
}
fn guard_runtime(
    runtime: &Path,
    config: &Value,
    plan: &Plan,
    touches_target: bool,
) -> Result<Kernel> {
    crate::daemon::verify_owner(runtime)?;
    ensure!(
        crate::keeper::status(runtime)?["alive"] != true,
        "stop the native Keeper before changing worktrees"
    );
    if let Some(start) = load(&runtime.join("kernel/native-start.json"))? {
        ensure!(
            !crate::startup::owner_alive(&start)?,
            "native startup is still active"
        );
    }
    let mut kernel = Kernel::open(runtime)?;
    let names = config["agents"]
        .as_object()
        .context("missing agents")?
        .iter()
        .filter(|(_, spec)| {
            spec["workspace_path"] == json!(plan.workspace_path)
                || touches_target && spec["workspace_path"] == json!(plan.source_root)
        })
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    ensure!(
        !names.is_empty(),
        "worktree no longer belongs to the committed configuration"
    );
    ensure!(crate::daemon::recovery_blockers(&mut kernel,&names)?.is_empty(),
        "worktree has unfinished jobs, replies, callbacks or provider faults; resolve them before lifecycle changes");
    let mount = load(&runtime.join("herdr/herdr-agents.json"))?.unwrap_or(Value::Null);
    for name in &names {
        let spec = &config["agents"][name];
        if spec["driver_kind"] == "herdr_tui" {
            let process = &mount["agents"][name]["binding"]["process"];
            ensure!(
                process.is_object()
                    && !crate::startup::owner_alive(
                        &json!({"pid":process["pid"],"process":process})
                    )?,
                "close the worktree's Herdr provider before lifecycle changes"
            );
            hosted_stopped(&mount["agents"][name])?;
        }
        if let Some(session) = kernel.store.json(&format!(
            "providers/{}/{name}/session.json",
            records::string(spec, "provider")?
        ))? {
            if let Some(pid) = session["pid"].as_u64() {
                ensure!(
                    pid <= u32::MAX as u64 && crate::daemon::owner_stopped(pid as u32),
                    "provider process is live or its exit cannot be confirmed"
                );
            }
        }
        let jobs = records::dispatch(
            &mut kernel.store,
            "latest",
            &json!({"stream":"jobs","identity":"job_id","agent_name":name}),
        )?;
        for job in jobs.as_array().unwrap() {
            ensure!(
                job["request"]["project_id"] == config["project_id"],
                "worktree job belongs to another project"
            );
            let proof = &job["provider_options"]["native_scope_proof"];
            if !proof.is_null() {
                ensure!(
                    crate::owned_process::probe_recovery(proof)["status"] == "stopped",
                    "worktree provider process scope is live or unconfirmed"
                );
            }
        }
    }
    Ok(kernel)
}
fn lock(path: &Path) -> Result<fs::File> {
    unlinked(path)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock()
        .context("worktree lifecycle already in progress")?;
    Ok(file)
}
fn repo_identity(plan: &Plan) -> Result<()> {
    ensure!(
        crate::config::project_id(&plan.source_root)? == plan.project_id
            && common(&plan.source_root)? == plan.common_dir,
        "worktree source repository changed"
    );
    unlinked(&plan.source_root)?;
    unlinked(&plan.workspace_path)?;
    Ok(())
}
fn actual(path: &Path, plan: &Plan, marker: &Value) -> Result<()> {
    unlinked(path)?;
    ensure!(
        path.is_dir()
            && common(path)? == plan.common_dir
            && absolute(
                path,
                Path::new(&git(path, &["rev-parse", "--show-toplevel"])?)
            )? == path,
        "worktree checkout identity changed"
    );
    ensure!(
        registered(&plan.source_root)?
            .iter()
            .any(|r| r.path == path && r.branch == plan.branch),
        "worktree registration/branch changed"
    );
    ensure!(
        load(&path.join(MARKER))?.as_ref() == Some(marker),
        "worktree ownership marker changed"
    );
    ensure!(
        git(path, &["ls-files", "-z", "--", MARKER])?.is_empty(),
        "ownership marker must not be tracked"
    );
    Ok(())
}
fn changes(path: &Path, source: bool, ignored: bool) -> Result<Vec<String>> {
    let mut args = vec!["status", "--porcelain=v1", "-z", "--untracked-files=all"];
    if ignored {
        args.push("--ignored");
    }
    let text = git(path, &args)?;
    let mut rows = Vec::new();
    let mut skip = false;
    for field in text.split('\0').filter(|s| !s.is_empty()) {
        if skip {
            skip = false;
            continue;
        }
        ensure!(field.len() >= 3, "invalid Git status record");
        let code = &field[..2];
        let name = &field[3..];
        skip = code.contains('R') || code.contains('C');
        if ["??", "!!"].contains(&code) && (name == MARKER || source && name.starts_with(".ccb/")) {
            continue;
        }
        rows.push(field.into());
    }
    Ok(rows)
}
fn checked_target(plan: &Plan, target: &str) -> Result<String> {
    validate_branch(&plan.source_root, target)?;
    ensure!(
        Some(target) != plan.branch.as_deref(),
        "cannot integrate a branch into itself"
    );
    ensure!(
        git(&plan.source_root, &["branch", "--show-current"])? == target,
        "source checkout must already be on the explicit target branch"
    );
    ensure!(
        changes(&plan.source_root, true, false)?.is_empty(),
        "target checkout has uncommitted changes"
    );
    revision(&plan.source_root, &format!("refs/heads/{target}"))
}
fn git_effect(path: &Path, args: &[&str], receipt: &Path) -> Result<Output> {
    let mut all = vec![
        "-c",
        "commit.gpgSign=false",
        "-c",
        "gc.auto=0",
        "-c",
        "submodule.recurse=false",
        "-c",
        "merge.autoStash=false",
    ];
    all.extend_from_slice(args);
    run(path, &all, Some(receipt))
}
fn effect(path: &Path, args: &[&str], receipt: &Path) -> Result<()> {
    let output = git_effect(path, args, receipt)?;
    ensure!(
        output.status.success(),
        "Git lifecycle operation failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}
fn persist(path: &Path, row: &mut Value, phase: &str, status: &str) -> Result<()> {
    row["phase"] = json!(phase);
    row["status"] = json!(status);
    row["updated_at"] = json!(crate::daemon::now());
    save(path, row)
}
fn parents(path: &Path) -> Result<Vec<String>> {
    Ok(git(path, &["rev-list", "--parents", "-n", "1", "HEAD"])?
        .split_whitespace()
        .skip(1)
        .map(str::to_owned)
        .collect())
}
fn stage_check(stage: &Path, plan: &Plan) -> Result<()> {
    unlinked(stage)?;
    ensure!(
        stage.is_dir()
            && common(stage)? == plan.common_dir
            && registered(&plan.source_root)?
                .iter()
                .any(|r| r.path == stage && r.branch.is_none()),
        "integration checkout identity changed"
    );
    Ok(())
}
fn merge(plan: &Plan, path: &Path, row: &mut Value) -> Result<()> {
    let target = records::string(row, "target_branch")?.to_owned();
    let before = records::string(row, "target_before")?.to_owned();
    let source = records::string(row, "source_commit")?.to_owned();
    let stage = PathBuf::from(records::string(row, "stage_path")?);
    let message = format!(
        "CCB integrate {}\n\nCCB-Operation: {}",
        plan.agent_name,
        records::string(row, "operation_id")?
    );
    if row["result_commit"].is_null() {
        actual(&plan.workspace_path, plan, &row["binding"])?;
        ensure!(
            revision(&plan.workspace_path, "HEAD")? == source
                && changes(&plan.workspace_path, false, false)?.is_empty(),
            "source worktree changed since merge intent"
        );
        if !stage.exists() {
            ensure!(
                !registered(&plan.source_root)?
                    .iter()
                    .any(|r| r.path == stage),
                "missing registered integration checkout"
            );
            persist(path, row, "stage_creating", "pending")?;
            effect(
                &plan.source_root,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    "--",
                    &portable(&stage)?,
                    &before,
                ],
                path,
            )?;
        }
        stage_check(&stage, plan)?;
        let head = revision(&stage, "HEAD")?;
        if head == before {
            let merge_head = run(&stage, &["rev-parse", "--verify", "MERGE_HEAD"], None)?;
            if merge_head.status.success() {
                ensure!(
                    String::from_utf8_lossy(&merge_head.stdout).trim() == source,
                    "integration MERGE_HEAD changed"
                );
                if !git(&stage, &["ls-files", "-u", "-z"])?.is_empty() {
                    return persist(path, row, "conflicted", "conflicted");
                }
                ensure!(
                    git(&stage, &["diff", "--name-only"])?.is_empty(),
                    "stage has unstaged conflict resolution; stage it before recover"
                );
                persist(path, row, "merging", "pending")?;
                effect(&stage, &["commit", "--no-verify", "-m", &message], path)?;
            } else {
                ensure!(
                    changes(&stage, false, true)?.is_empty(),
                    "unconfirmed integration checkout changes"
                );
                persist(path, row, "merging", "pending")?;
                let output = git_effect(
                    &stage,
                    &[
                        "merge",
                        "--no-ff",
                        "--no-overwrite-ignore",
                        "--no-edit",
                        "-m",
                        &message,
                        &source,
                    ],
                    path,
                )?;
                if !output.status.success() {
                    if !git(&stage, &["ls-files", "-u", "-z"])?.is_empty() {
                        return persist(path, row, "conflicted", "conflicted");
                    }
                    anyhow::bail!(
                        "Git merge failed; original target preserved: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
            }
        }
        let result = revision(&stage, "HEAD")?;
        ensure!(
            parents(&stage)? == vec![before.clone(), source.clone()]
                && git(&stage, &["log", "-1", "--format=%B"])?.trim() == message
                && changes(&stage, false, true)?.is_empty(),
            "integration result does not match the recorded operation"
        );
        row["result_commit"] = json!(result);
        persist(path, row, "merge_committed", "pending")?;
    }
    let result = records::string(row, "result_commit")?.to_owned();
    if result == before {
        ensure!(
            run(
                &plan.source_root,
                &["merge-base", "--is-ancestor", &source, &before],
                None
            )?
            .status
            .success(),
            "recorded no-op merge is not integrated"
        );
    } else {
        let ancestry = git(
            &plan.source_root,
            &["rev-list", "--parents", "-n", "1", &result],
        )?;
        ensure!(
            ancestry.split_whitespace().collect::<Vec<_>>()
                == vec![result.as_str(), before.as_str(), source.as_str()]
                && git(&plan.source_root, &["log", "-1", "--format=%B", &result])?.trim()
                    == message,
            "recorded merge commit does not match its intent"
        );
    }
    // Verify retained operator files before publishing, not only before cleanup.
    if stage.exists() {
        stage_check(&stage, plan)?;
        ensure!(
            revision(&stage, "HEAD")? == result && changes(&stage, false, true)?.is_empty(),
            "integration stage changed; target publication is blocked"
        );
    } else {
        ensure!(
            !registered(&plan.source_root)?
                .iter()
                .any(|r| r.path == stage),
            "integration stage registration is unconfirmed"
        );
    }
    let current = checked_target(plan, &target)?;
    ensure!(
        current == before || current == result,
        "target branch moved; merge publication is blocked"
    );
    if current == before {
        persist(path, row, "publishing", "pending")?;
        effect(
            &plan.source_root,
            &[
                "merge",
                "--ff-only",
                "--no-overwrite-ignore",
                "--no-edit",
                &result,
            ],
            path,
        )?;
    }
    ensure!(
        revision(&plan.source_root, "HEAD")? == result,
        "target publication is unconfirmed"
    );
    if stage.exists() {
        stage_check(&stage, plan)?;
        ensure!(
            revision(&stage, "HEAD")? == result && changes(&stage, false, true)?.is_empty(),
            "integration stage cannot be cleaned"
        );
        persist(path, row, "stage_removing", "pending")?;
        effect(
            &plan.source_root,
            &["worktree", "remove", "--", &portable(&stage)?],
            path,
        )?;
    }
    ensure!(
        !registered(&plan.source_root)?
            .iter()
            .any(|r| r.path == stage),
        "integration stage cleanup unconfirmed"
    );
    persist(path, row, "completed", "completed")?;
    pointer(plan, row, "available")
}
fn archive_binding(plan: &Plan, archive: &Path) -> Result<Value> {
    let mut value = plan.binding()?;
    value["workspace_path"] = json!(portable(archive)?);
    Ok(value)
}
fn relocate(plan: &Plan, path: &Path, row: &mut Value, restoring: bool) -> Result<()> {
    pin(plan, row, path)?;
    let archive = PathBuf::from(records::string(row, "archive_path")?);
    let (from, to, old_marker, new_marker) = if restoring {
        (
            archive.clone(),
            plan.workspace_path.clone(),
            archive_binding(plan, &archive)?,
            plan.binding()?,
        )
    } else {
        (
            plan.workspace_path.clone(),
            archive.clone(),
            plan.binding()?,
            archive_binding(plan, &archive)?,
        )
    };
    unlinked(&from)?;
    unlinked(&to)?;
    let rows = registered(&plan.source_root)?;
    if rows.iter().any(|r| r.path == from) {
        ensure!(
            !to.exists() && !rows.iter().any(|r| r.path == to),
            "worktree move destination is occupied"
        );
        actual(&from, plan, &old_marker)?;
        ensure!(
            revision(&from, "HEAD")? == row["source_commit"],
            "worktree HEAD changed before move"
        );
        persist(path, row, "moving", "pending")?;
        effect(
            &plan.source_root,
            &["worktree", "move", "--", &portable(&from)?, &portable(&to)?],
            path,
        )?;
    }
    ensure!(
        !from.exists()
            && !registered(&plan.source_root)?
                .iter()
                .any(|r| r.path == from),
        "worktree move outcome is unconfirmed"
    );
    let observed = load(&to.join(MARKER))?.context("moved worktree marker missing")?;
    ensure!(
        observed == old_marker || observed == new_marker,
        "moved worktree ownership changed"
    );
    actual(&to, plan, &observed)?;
    ensure!(
        revision(&to, "HEAD")? == row["source_commit"],
        "moved worktree HEAD changed"
    );
    save(&to.join(MARKER), &new_marker)?;
    persist(path, row, "completed", "completed")?;
    pointer(plan, row, if restoring { "available" } else { "retired" })
}
fn purge(plan: &Plan, path: &Path, row: &mut Value) -> Result<()> {
    pin(plan, row, path)?;
    let archive = PathBuf::from(records::string(row, "archive_path")?);
    let target = records::string(row, "target_branch")?.to_owned();
    let saved = records::string(row, "source_commit")?.to_owned();
    let target_commit = checked_target(plan, &target)?;
    ensure!(
        archive.exists()
            || ["removing", "branch_removing"].contains(&row["phase"].as_str().unwrap_or("")),
        "archive disappeared before removal intent; inspect explicitly"
    );
    ensure!(
        run(
            &plan.source_root,
            &["merge-base", "--is-ancestor", &saved, &target_commit],
            None
        )?
        .status
        .success(),
        "archive commits are not integrated into target"
    );
    if archive.exists() {
        let marker = load(&archive.join(MARKER))?;
        if marker.is_some() {
            actual(&archive, plan, &archive_binding(plan, &archive)?)?;
        } else {
            ensure!(
                row["phase"] == "removing",
                "archive ownership marker disappeared"
            );
            ensure!(
                common(&archive)? == plan.common_dir
                    && registered(&plan.source_root)?
                        .iter()
                        .any(|r| r.path == archive && r.branch == plan.branch),
                "archive registration changed"
            );
        }
        ensure!(revision(&archive,"HEAD")? == saved && changes(&archive,false,true)?.is_empty(),
            "archive contains modified, untracked or ignored user files; restore or clean it explicitly");
        persist(path, row, "removing", "pending")?;
        if marker.is_some() {
            fs::remove_file(archive.join(MARKER))?;
        }
        effect(
            &plan.source_root,
            &["worktree", "remove", "--", &portable(&archive)?],
            path,
        )?;
    }
    ensure!(
        !registered(&plan.source_root)?
            .iter()
            .any(|r| r.path == archive),
        "archive removal is unconfirmed"
    );
    if row["delete_branch"] == true {
        let branch = plan.branch.as_deref().context("managed branch missing")?;
        if branch_exists(&plan.source_root, branch)? {
            ensure!(
                revision(&plan.source_root, &format!("refs/heads/{branch}"))? == saved
                    && !registered(&plan.source_root)?
                        .iter()
                        .any(|r| r.branch.as_deref() == Some(branch)),
                "branch changed or is checked out elsewhere"
            );
            persist(path, row, "branch_removing", "pending")?;
            effect(
                &plan.source_root,
                &["update-ref", "-d", &format!("refs/heads/{branch}"), &saved],
                path,
            )?;
        }
    }
    persist(path, row, "completed", "completed")?;
    pointer(plan, row, "retired")
}

pub(crate) fn execute(
    runtime: &Path,
    project: Option<&Path>,
    action: &str,
    target: &str,
    options: &Value,
) -> Result<Value> {
    ensure!(
        ["inspect", "merge", "retire", "restore", "purge", "recover", "status"].contains(&action),
        "unknown worktree action"
    );
    if ["inspect", "status", "recover"].contains(&action) {
        ensure!(
            options.as_object().is_some_and(|o| o.is_empty()),
            "read/recover commands do not accept mutation options"
        );
    } else {
        ensure!(
            action == "purge" || options["delete_branch"].is_null(),
            "--delete-branch is only supported by purge"
        );
        ensure!(
            ["merge", "purge"].contains(&action) || options["into"].is_null(),
            "--into is only supported by merge/purge"
        );
    }
    let runtime = absolute(&std::env::current_dir()?, runtime)?;
    let config = config(&runtime)?;
    if let Some(project) = project {
        ensure!(
            crate::config::project_id(project)? == config["project_id"],
            "worktree project mismatch"
        );
    }
    let (previous, plan) = if ["restore", "purge", "recover", "status"].contains(&action) {
        let (path, row, plan) = operation(&runtime, target)?;
        (Some((path, row)), plan)
    } else {
        (None, selected(&config, target)?)
    };
    ensure!(
        config["project_id"] == plan.project_id,
        "worktree runtime belongs to another project"
    );
    ensure!(
        selected(&config, &plan.agent_name)? == plan,
        "operation plan no longer matches the committed configuration"
    );
    repo_identity(&plan)?;
    if action == "status" {
        return Ok(previous.unwrap().1);
    }
    if action == "inspect" {
        return Ok(
            json!({"binding":plan.binding()?,"lifecycle":load(&plan.sibling(POINTER))?,
            "head":if plan.workspace_path.exists(){json!(revision(&plan.workspace_path,"HEAD")?)}else{Value::Null},
            "changes":if plan.workspace_path.exists(){json!(changes(&plan.workspace_path,false,true)?)}else{Value::Null},
            "source_branch":git(&plan.source_root,&["branch","--show-current"])?}),
        );
    }
    let touches_target = ["merge", "purge"].contains(&action)
        || action == "recover"
            && previous.as_ref().is_some_and(|(_, r)| {
                ["merge", "purge"].contains(&r["action"].as_str().unwrap_or(""))
            });
    let _authority = guard_runtime(&runtime, &config, &plan, touches_target)?;
    let _repo_lock = lock(&plan.common_dir.join("ccb-native-lifecycle.lock"))?;
    let parent = plan
        .workspace_path
        .parent()
        .context("missing worktree parent")?;
    fs::create_dir_all(parent)?;
    let _workspace_lock = lock(&plan.sibling("worktree.lock"))?;
    if action == "recover" {
        let (path, mut row) = previous.unwrap();
        if row["status"] == "completed" {
            finish_pointer(&plan, &row)?;
            return Ok(row);
        }
        claim(&plan, &row)?;
        match records::string(&row, "action")? {
            "merge" => merge(&plan, &path, &mut row)?,
            "retire" => relocate(&plan, &path, &mut row, false)?,
            "restore" => relocate(&plan, &path, &mut row, true)?,
            "purge" => purge(&plan, &path, &mut row)?,
            _ => anyhow::bail!("invalid recorded worktree action"),
        }
        return Ok(row);
    }
    let operation = if let Some(s) = options["operation_id"].as_str() {
        id(s)?
    } else {
        uuid::Uuid::new_v4().to_string()
    };
    let dir = op_root(&runtime)?.join(&operation);
    let path = dir.join("operation.json");
    if load(&path)?.is_some() {
        let (_, existing, _) = self::operation(&runtime, &operation)?;
        ensure!(
            existing["action"] == action
                && existing["binding"] == plan.binding()?
                && existing["plan"] == json!(plan)
                && existing["target_branch"] == options["into"]
                && existing["delete_branch"] == (options["delete_branch"] == true)
                && (!["restore", "purge"].contains(&action) || existing["retirement_id"] == target),
            "operation ID already has different worktree arguments"
        );
        if existing["status"] == "completed" {
            finish_pointer(&plan, &existing)?;
            return Ok(existing);
        }
        anyhow::bail!("worktree operation already exists; use recover with its ID");
    }
    let mut row = json!({"schema_version":1,"record_type":"native_worktree_operation","operation_id":operation,
        "runtime_root":runtime,"project_id":plan.project_id,"common_dir":plan.common_dir,"plan":plan,
        "binding":plan.binding()?,"action":action,"phase":"prepared","status":"pending","created_at":crate::daemon::now(),
        "stage_path":dir.join("integration"),"target_branch":options["into"],"delete_branch":options["delete_branch"] == true});
    if action == "merge" || action == "retire" {
        startup_guard(&plan)?;
        actual(&plan.workspace_path, &plan, &plan.binding()?)?;
        row["source_commit"] = json!(revision(&plan.workspace_path, "HEAD")?);
        if action == "merge" {
            ensure!(
                changes(&plan.workspace_path, false, false)?.is_empty(),
                "source worktree has uncommitted changes"
            );
            row["target_before"] = json!(checked_target(&plan, records::string(options, "into")?)?);
            let ancestor = run(
                &plan.source_root,
                &[
                    "merge-base",
                    "--is-ancestor",
                    records::string(&row, "source_commit")?,
                    records::string(&row, "target_before")?,
                ],
                None,
            )?;
            ensure!(
                ancestor.status.success() || ancestor.status.code() == Some(1),
                "cannot check merge ancestry"
            );
            if ancestor.status.success() {
                row["result_commit"] = row["target_before"].clone();
            }
        } else {
            let archive = archive_path(&plan, &operation)?;
            ensure!(!archive.exists(), "archive path already exists");
            row["archive_path"] = json!(archive);
        }
    } else {
        let (_, retired) = previous.context("missing retirement receipt")?;
        ensure!(
            retired["action"] == "retire" && retired["status"] == "completed",
            "restore/purge requires a completed retirement"
        );
        let current = load(&plan.sibling(POINTER))?.context("retired lifecycle pointer missing")?;
        ensure!(
            current["state"] == "retired" && current["operation_id"] == retired["operation_id"],
            "retirement is no longer current"
        );
        row["archive_path"] = retired["archive_path"].clone();
        row["source_commit"] = retired["source_commit"].clone();
        row["retirement_id"] = retired["operation_id"].clone();
        let archive = PathBuf::from(records::string(&row, "archive_path")?);
        actual(&archive, &plan, &archive_binding(&plan, &archive)?)?;
        ensure!(
            revision(&archive, "HEAD")? == row["source_commit"],
            "archive HEAD changed; inspect it explicitly"
        );
        if action == "purge" {
            ensure!(
                changes(&archive, false, true)?.is_empty(),
                "archive still contains user changes or ignored files"
            );
            let target_head = checked_target(&plan, records::string(options, "into")?)?;
            ensure!(
                run(
                    &plan.source_root,
                    &[
                        "merge-base",
                        "--is-ancestor",
                        records::string(&row, "source_commit")?,
                        &target_head
                    ],
                    None
                )?
                .status
                .success(),
                "archive commits are not integrated into target"
            );
        } else {
            ensure!(
                action == "restore" && !plan.workspace_path.exists(),
                "restore destination is occupied"
            );
        }
    }
    if options["dry_run"] == true {
        row["status"] = json!("planned");
        return Ok(row);
    }
    row["previous_pointer"] = load(&plan.sibling(POINTER))?.unwrap_or(Value::Null);
    unlinked(&dir)?;
    fs::create_dir_all(&dir)?;
    save(&path, &row)?;
    pointer(&plan, &row, "pending")?;
    match action {
        "merge" => merge(&plan, &path, &mut row)?,
        "retire" => relocate(&plan, &path, &mut row, false)?,
        "restore" => relocate(&plan, &path, &mut row, true)?,
        "purge" => purge(&plan, &path, &mut row)?,
        _ => anyhow::bail!("invalid worktree lifecycle action"),
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_pane_does_not_authorize_cleanup_of_a_live_hosted_provider() -> Result<()> {
        let process = crate::owned_process::observe_external_process(std::process::id())?;
        for key in ["host_process", "server_process"] {
            let mut row = json!({"host_launch":{}});
            row["host_launch"][key] = process.clone();
            ensure!(
                hosted_stopped(&row).is_err(),
                "live hosted provider was accepted"
            );
        }
        ensure!(hosted_stopped(&json!({"host_launch":{"host_process":"corrupt"}})).is_err());
        hosted_stopped(&json!({}))
    }
}
