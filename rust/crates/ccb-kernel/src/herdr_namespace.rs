//! Durable allocation of owned Herdr shell panes. Provider launch is separate.
//! Every mutation has a committed intent; uncertain creation is never replayed.
use crate::{herdr_transport, store::Store};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
};

const RECORD: &str = "namespace.json";
pub(crate) fn unlinked(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "linked namespace authority path"
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    metadata.file_attributes() & 0x400 == 0,
                    "reparse namespace authority path"
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
fn validate_record(record: &Value, plan: &Value, signature: &str) -> Result<()> {
    ensure!(
        record.get("pending").is_some() && record.get("workspace_id").is_some(),
        "namespace ledger is missing lifecycle fields"
    );
    ensure!(
        record["schema_version"] == 1
            && record["record_type"] == "native_herdr_namespace"
            && record["project_id"] == plan["project_id"]
            && record["session"] == plan["session"]
            && record["plan_hash"] == signature
            && record["plan"] == *plan,
        "namespace plan/session/project changed; explicit migration required"
    );
    let owner = text(record, "owner_id")?;
    ensure!(
        uuid::Uuid::parse_str(owner).is_ok() && record["label"] == format!("ccb-native-{owner}"),
        "invalid namespace owner identity"
    );
    ensure!(
        record["workspace_tagged"].is_boolean()
            && ["preparing", "uncertain", "prepared", "retiring", "retired"]
                .contains(&record["status"].as_str().unwrap_or("")),
        "invalid namespace ledger state"
    );
    let bindings = record["bindings"]
        .as_object()
        .context("invalid namespace bindings")?;
    let mut ids = BTreeSet::new();
    for (slot, binding) in bindings {
        let id = text(binding, "pane_id")?;
        ensure!(
            plan["slots"].get(slot).is_some()
                && !id.is_empty()
                && id.len() <= 128
                && binding["tagged"].is_boolean()
                && ids.insert(id),
            "invalid namespace binding identity"
        );
    }
    if record["workspace_id"].is_null() {
        ensure!(
            bindings.is_empty() && record["workspace_tagged"] == false,
            "invalid unallocated namespace"
        );
    } else {
        ensure!(
            !text(record, "workspace_id")?.is_empty()
                && bindings.contains_key(text(plan, "root_slot")?),
            "invalid allocated namespace"
        );
    }
    let pending = &record["pending"];
    if !pending.is_null() {
        let kind = text(pending, "kind")?;
        ensure!(
            uuid::Uuid::parse_str(text(pending, "operation_id")?).is_ok()
                && pending["sent"].is_boolean(),
            "invalid namespace intent identity"
        );
        ensure!(
            ["create_workspace", "split", "tag_workspace", "tag_pane"].contains(&kind),
            "invalid namespace intent kind"
        );
        if ["split", "tag_pane"].contains(&kind) {
            ensure!(
                plan["slots"].get(text(pending, "slot")?).is_some(),
                "invalid namespace intent slot"
            );
        }
        if ["create_workspace", "split"].contains(&kind) {
            ensure!(
                pending["baseline"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().all(Value::is_string)),
                "invalid namespace creation baseline"
            );
        }
    }
    if ["prepared", "retiring", "retired"].contains(&record["status"].as_str().unwrap_or("")) {
        ensure!(
            pending.is_null()
                && record["workspace_tagged"] == true
                && bindings.len() == plan["slots"].as_object().unwrap().len()
                && bindings.values().all(|b| b["tagged"] == true),
            "incomplete prepared namespace"
        );
    }
    if ["retiring", "retired"].contains(&record["status"].as_str().unwrap_or("")) {
        let retirement = &record["retirement"];
        ensure!(
            retirement["schema_version"] == 1
                && retirement["owner_id"] == record["owner_id"]
                && retirement["workspace_id"] == record["workspace_id"]
                && retirement["close_sent"].is_boolean()
                && uuid::Uuid::parse_str(text(retirement, "operation_id")?).is_ok(),
            "invalid namespace retirement identity"
        );
    }
    Ok(())
}
#[derive(Debug)]
struct Persistence(anyhow::Error);
impl std::fmt::Display for Persistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "namespace persistence failed: {}", self.0)
    }
}
impl std::error::Error for Persistence {}
fn hash(v: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(v)?)))
}
pub(crate) fn launch_env(request: &Value, slot: &str) -> Result<serde_json::Map<String, Value>> {
    let mut env = match &request["config"]["agents"][slot]["env"] {
        Value::Null => serde_json::Map::new(),
        Value::Object(env) => env.clone(),
        _ => anyhow::bail!("invalid namespace environment"),
    };
    if !request["slot_env"].is_null() {
        let slots = request["slot_env"]
            .as_object()
            .context("slot_env must be an object")?;
        if let Some(value) = slots.get(slot) {
            env.extend(
                value
                    .as_object()
                    .context("slot environment must be an object")?
                    .clone(),
            );
        }
    }
    ensure!(
        env.len() <= 32,
        "namespace environment has too many entries"
    );
    let mut seen = BTreeSet::new();
    let mut size = 0;
    for (key, value) in &env {
        let value = value
            .as_str()
            .context("namespace environment values must be strings")?;
        ensure!(
            !key.is_empty()
                && key.len() <= 128
                && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                && !key.as_bytes()[0].is_ascii_digit()
                && !key.to_ascii_uppercase().starts_with("HERDR_")
                && !value.contains('\0'),
            "invalid or Herdr-owned environment variable"
        );
        ensure!(
            seen.insert(if cfg!(windows) {
                key.to_ascii_uppercase()
            } else {
                key.clone()
            }),
            "ambiguous namespace environment names"
        );
        size += key.len() + value.len();
    }
    ensure!(size <= 16384, "namespace environment is too large");
    Ok(env)
}
fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str().context("invalid namespace string field")
}
fn portable(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(v) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{v}")
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(&s).into()
    }
}
fn leaf(name: &str) -> Value {
    json!({"kind":"leaf","name":name,"percent":null})
}
fn stack(names: &[String]) -> Value {
    names[1..].iter().fold(
        leaf(&names[0]),
        |left, n| json!({"kind":"vertical","left":left,"right":leaf(n)}),
    )
}
fn first(n: &Value) -> &str {
    if n["kind"] == "leaf" {
        n["name"].as_str().unwrap()
    } else {
        first(&n["left"])
    }
}
fn names(n: &Value, out: &mut Vec<String>) {
    if n["kind"] == "leaf" {
        out.push(first(n).into());
    } else {
        names(&n["left"], out);
        names(&n["right"], out);
    }
}
fn percent(n: &Value) -> Option<u64> {
    if n["kind"] == "leaf" {
        n["percent"].as_u64()
    } else {
        percent(&n["left"]).or_else(|| percent(&n["right"]))
    }
}
fn count(n: &Value) -> u64 {
    if n["kind"] == "leaf" {
        1
    } else {
        count(&n["left"]) + count(&n["right"])
    }
}
fn splits(n: &Value, out: &mut Vec<Value>) {
    if n["kind"] == "leaf" {
        return;
    }
    let (num, total) = (100 * count(&n["right"]), count(n));
    let (q, r) = (num / total, num % total);
    let rounded = q + u64::from(2 * r > total || 2 * r == total && q % 2 == 1);
    let pct = percent(&n["right"])
        .or_else(|| percent(&n["left"]).map(|n| 100 - n))
        .unwrap_or(rounded)
        .clamp(1, 99);
    out.push(json!({"slot":first(&n["right"]),"parent":first(n),"direction":if n["kind"]=="horizontal"{"right"}else{"bottom"},"percent":pct}));
    splits(&n["left"], out);
    splits(&n["right"], out);
}
pub fn plan(request: &Value) -> Result<Value> {
    ensure!(request["schema_version"] == 1, "invalid namespace schema");
    let config = &request["config"];
    crate::daemon::Daemon::validate_config(config)?;
    let supplied = Path::new(text(config, "project_root")?);
    ensure!(
        supplied.is_absolute(),
        "namespace project root must be absolute"
    );
    let root = supplied.canonicalize()?;
    ensure!(
        config["project_id"] == crate::config::project_id(&root)?,
        "namespace project identity mismatch"
    );
    let session = text(request, "session")?;
    ensure!(
        !session.is_empty()
            && session.len() <= 512
            && !session.starts_with('-')
            && session.trim() == session
            && !session.chars().any(char::is_control),
        "invalid namespace session"
    );
    let command: Vec<String> = serde_json::from_value(request["command"].clone())
        .map_err(|_| anyhow::anyhow!("invalid namespace executable argv"))?;
    ensure!(
        !command.is_empty()
            && command.len() <= 16
            && command.iter().all(|s| !s.is_empty() && !s.contains('\0')),
        "invalid namespace executable argv"
    );
    let timeout = if let Some(value) = request.get("timeout_ms") {
        value.as_u64().context("invalid namespace timeout")?
    } else {
        10000
    };
    ensure!((50..=60000).contains(&timeout), "invalid namespace timeout");
    let source = &config["source_config"];
    let window_layout = crate::config::single_window_layout(source)?;
    let agents = config["agents"].as_object().unwrap();
    let mut ordered: Vec<String> = if source["default_agents"].is_array() {
        serde_json::from_value(source["default_agents"].clone())?
    } else {
        agents.keys().cloned().collect()
    };
    ensure!(
        !ordered.is_empty()
            && ordered.iter().collect::<BTreeSet<_>>().len() == ordered.len()
            && ordered.iter().all(|n| agents.contains_key(n)),
        "invalid namespace default agents"
    );
    let layout = source.get("layout").or_else(|| source.get("layout_spec"));
    let tree = if let Some(window) = window_layout {
        crate::config::layout_tree(window, &root, &config["agents"])?
    } else if let Some(layout) = layout.filter(|v| !v.is_null()) {
        crate::config::layout_tree(
            layout.as_str().context("layout must be text")?,
            &root,
            &config["agents"],
        )?
    } else {
        if source["cmd_enabled"] == true {
            ordered.insert(0, "cmd".into());
        }
        if ordered.len() == 1 {
            leaf(&ordered[0])
        } else {
            let mid = ordered.len().div_ceil(2);
            json!({"kind":"horizontal","left":stack(&ordered[..mid]),"right":stack(&ordered[mid..])})
        }
    };
    let mut visible = Vec::new();
    names(&tree, &mut visible);
    ensure!(
        visible.len() <= 33 && visible.iter().collect::<BTreeSet<_>>().len() == visible.len(),
        "invalid namespace leaves"
    );
    ensure!(
        agents.keys().all(|n| visible.contains(n))
            && visible.iter().all(|n| n == "cmd" || agents.contains_key(n)),
        "namespace layout must cover configured agents exactly"
    );
    ensure!(
        !visible.iter().any(|n| n == "cmd") || visible.first().is_some_and(|n| n == "cmd"),
        "cmd must be the first pane"
    );
    if let Some(enabled) = source["cmd_enabled"].as_bool() {
        ensure!(
            enabled == visible.iter().any(|n| n == "cmd"),
            "namespace cmd flag and layout disagree"
        );
    }
    let mut slots = json!({});
    if let Some(env) = request["slot_env"].as_object() {
        ensure!(
            env.keys().all(|name| visible.contains(name)),
            "environment targets an unknown namespace slot"
        );
    }
    for name in &visible {
        let spec = &config["agents"][name];
        let cwd = spec["workspace_path"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| root.clone());
        ensure!(cwd.is_absolute(), "namespace workspace must be absolute");
        slots[name] = json!({"cwd":portable(&cwd),"provider":spec["provider"],"role":spec["role"]});
        let env = launch_env(request, name)?;
        if !env.is_empty() {
            slots[name]["environment_hash"] = json!(hash(&json!(env))?);
        }
    }
    let mut steps = Vec::new();
    splits(&tree, &mut steps);
    Ok(
        json!({"project_id":config["project_id"],"project_root":portable(&root),"session":session,
        "command_hash":hash(&json!([command,std::env::var_os("HERDR_CONFIG_PATH"),std::env::var_os("USERPROFILE"),std::env::var_os("HOME"),std::env::var_os("XDG_CONFIG_HOME")]))?,"root_slot":first(&tree),
        "visible_slots":visible,"slots":slots,"splits":steps,"layout":tree}),
    )
}
struct Controller<'a> {
    store: Store,
    request: &'a Value,
    plan: Value,
    record: Value,
}
impl Controller<'_> {
    fn save(&mut self) -> Result<()> {
        (|| {
            if self.store.json(RECORD)?.as_ref() != Some(&self.record) {
                self.store.save(RECORD, &self.record)?;
            }
            self.store.commit()
        })()
        .map_err(|error| Persistence(error).into())
    }
    fn rpc(&self, args: Vec<String>) -> Result<Value> {
        let metadata = args.get(1).is_some_and(|value| value == "report-metadata");
        let response = herdr_transport::execute(
            &json!({"schema_version":1,"operation":"namespace","session":self.plan["session"],"command":self.request["command"],"args":args,"expect_json":!metadata,"timeout_ms":self.request["timeout_ms"].as_u64().unwrap_or(10000)}),
        )?;
        ensure!(
            response["ok"] == true,
            "Herdr namespace command outcome is unknown"
        );
        // Herdr metadata commands may acknowledge with empty/plain stdout.
        // The caller still verifies the actual tokens via a subsequent read.
        if metadata {
            return Ok(Value::Null);
        }
        let value: Value = serde_json::from_str(
            response["stdout"]
                .as_str()
                .context("missing Herdr response")?,
        )?;
        Ok(if value["result"].is_object() {
            value["result"].clone()
        } else {
            value
        })
    }
    fn workspaces(&self) -> Result<Vec<Value>> {
        serde_json::from_value(
            self.rpc(vec!["workspace".into(), "list".into()])?["workspaces"].clone(),
        )
        .context("invalid workspace inventory")
    }
    fn panes(&self) -> Result<Vec<Value>> {
        serde_json::from_value(
            self.rpc(vec![
                "pane".into(),
                "list".into(),
                "--workspace".into(),
                text(&self.record, "workspace_id")?.into(),
            ])?["panes"]
                .clone(),
        )
        .context("invalid pane inventory")
    }
    fn owns(&self, row: &Value, slot: Option<&str>) -> bool {
        row["tokens"]["ccb_native_owner"] == self.record["owner_id"]
            && row["tokens"]["ccb_project_id"] == self.plan["project_id"]
            && row["tokens"]["ccb_namespace_id"] == self.record["workspace_id"]
            && slot.is_none_or(|s| row["tokens"]["ccb_slot"] == s)
    }
    fn workspace(&self, tagged: bool) -> Result<Value> {
        let rows: Vec<_> = self
            .workspaces()?
            .into_iter()
            .filter(|w| w["workspace_id"] == self.record["workspace_id"])
            .collect();
        ensure!(
            rows.len() == 1 && rows[0]["label"] == self.record["label"],
            "namespace workspace changed or disappeared"
        );
        ensure!(
            if tagged {
                self.owns(&rows[0], None)
            } else {
                rows[0]["tokens"]["ccb_native_owner"].is_null() || self.owns(&rows[0], None)
            },
            "namespace workspace has another owner"
        );
        Ok(rows[0].clone())
    }
    fn binding_visible(&self, slot: &str, tagged: bool) -> Result<bool> {
        Ok(self.binding_in_inventory(&self.panes()?, slot, tagged))
    }
    fn binding_in_inventory(&self, panes: &[Value], slot: &str, tagged: bool) -> bool {
        let binding = &self.record["bindings"][slot];
        let rows: Vec<_> = panes
            .iter()
            .filter(|p| {
                p["pane_id"] == binding["pane_id"]
                    && p["workspace_id"] == self.record["workspace_id"]
            })
            .collect();
        rows.len() == 1
            && if tagged {
                self.owns(rows[0], Some(slot))
            } else {
                rows[0]["tokens"]["ccb_native_owner"].is_null() || self.owns(rows[0], Some(slot))
            }
    }
    fn validate_bound(&self) -> Result<()> {
        self.workspace(true)?;
        // One live inventory covers every slot in this validation pass. A
        // subsequent mutation still requires a new post-operation observation.
        let panes = self.panes()?;
        for (slot, binding) in self.record["bindings"]
            .as_object()
            .context("invalid namespace bindings")?
        {
            ensure!(
                self.binding_in_inventory(&panes, slot, binding["tagged"] == true),
                "namespace pane changed or disappeared"
            );
        }
        Ok(())
    }
    fn retire(&mut self, pre_backend: bool) -> Result<Value> {
        ensure!(
            ["prepared", "retiring", "retired"]
                .contains(&self.record["status"].as_str().unwrap_or("")),
            "incomplete namespace requires explicit repair before retirement"
        );
        let present = self
            .workspaces()?
            .iter()
            .any(|w| w["workspace_id"] == self.record["workspace_id"]);
        if present {
            ensure!(
                self.record["status"] != "retired",
                "retired workspace identity reappeared; refusing another close"
            );
            self.validate_bound()?;
            let panes = self.panes()?;
            let bindings = self.record["bindings"].as_object().unwrap();
            ensure!(
                panes.len() == bindings.len()
                    && panes.iter().all(|p| bindings
                        .iter()
                        .any(|(slot, b)| p["pane_id"] == b["pane_id"] && self.owns(p, Some(slot)))),
                "namespace contains untracked or foreign panes"
            );
        }
        if self.record["retirement"].is_null() {
            self.record["retirement"] = json!({"schema_version":1,"operation_id":uuid::Uuid::new_v4().to_string(),
                "owner_id":self.record["owner_id"],"workspace_id":self.record["workspace_id"],"close_sent":present});
            if pre_backend {
                self.record["retirement"]["pre_backend"] = json!(true);
            }
            self.record["status"] = json!(if present { "retiring" } else { "retired" });
            // The committed permission precedes IO. An uncertain send is only
            // observed on later calls, never replayed against a reused ID.
            self.save()?;
            if present {
                let _ = self.rpc(vec![
                    "workspace".into(),
                    "close".into(),
                    text(&self.record, "workspace_id")?.into(),
                ]);
            }
        }
        if self.record["status"] != "retired"
            && !self
                .workspaces()?
                .iter()
                .any(|w| w["workspace_id"] == self.record["workspace_id"])
        {
            self.record["status"] = json!("retired");
            self.save()?;
        }
        Ok(json!({"namespace":self.record,"provider_commands_sent":false}))
    }
    fn begin(&mut self, kind: &str, slot: Option<&str>, baseline: Value) -> Result<()> {
        self.record["pending"] = json!({"operation_id":uuid::Uuid::new_v4().to_string(),"kind":kind,"slot":slot,"baseline":baseline,"sent":false});
        self.record["status"] = json!("preparing");
        self.save()
    }
    fn uncertain(&mut self) -> Result<Value> {
        self.record["status"] = json!("uncertain");
        self.save()?;
        Ok(self.record.clone())
    }
    fn observe_pending(&mut self) -> Result<bool> {
        let pending = self.record["pending"].clone();
        let slot = pending["slot"].as_str();
        match pending["kind"].as_str().unwrap_or("") {
            "tag_workspace" => {
                if self.workspace(true).is_err() {
                    return Ok(false);
                }
                self.record["workspace_tagged"] = json!(true);
            }
            "tag_pane" => {
                self.workspace(true)?;
                let slot = slot.context("missing pending pane slot")?;
                if !self.binding_visible(slot, true)? {
                    return Ok(false);
                }
                self.record["bindings"][slot]["tagged"] = json!(true);
            }
            "create_workspace" | "split" => return Ok(false),
            _ => anyhow::bail!("invalid pending namespace operation"),
        }
        self.record["pending"] = Value::Null;
        self.save()?;
        Ok(true)
    }
    fn next(&mut self) -> Result<bool> {
        if self.record["workspace_id"].is_null() {
            let rows = self.workspaces()?;
            self.begin(
                "create_workspace",
                None,
                json!(rows
                    .iter()
                    .map(|w| w["workspace_id"].clone())
                    .collect::<Vec<_>>()),
            )?;
            return Ok(true);
        }
        if self.record["workspace_tagged"] != true {
            self.workspace(false)?;
            self.begin("tag_workspace", None, Value::Null)?;
            return Ok(true);
        }
        self.validate_bound()?;
        let untagged = self.record["bindings"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, v)| v["tagged"] != true)
            .map(|(k, _)| k.clone());
        if let Some(slot) = untagged {
            self.begin("tag_pane", Some(&slot), Value::Null)?;
            return Ok(true);
        }
        let step = self.plan["splits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| {
                self.record["bindings"]
                    .get(s["slot"].as_str().unwrap())
                    .is_none()
            })
            .cloned();
        if let Some(step) = step {
            let rows = self.panes()?;
            self.begin(
                "split",
                Some(text(&step, "slot")?),
                json!(rows
                    .iter()
                    .map(|p| p["pane_id"].clone())
                    .collect::<Vec<_>>()),
            )?;
            return Ok(true);
        }
        self.record["status"] = json!("prepared");
        self.save()?;
        Ok(false)
    }
    fn apply_pending(&mut self) -> Result<()> {
        let pending = self.record["pending"].clone();
        let kind = text(&pending, "kind")?;
        let slot = pending["slot"].as_str();
        let mut args = match kind {
            "create_workspace" => vec![
                "workspace".into(),
                "create".into(),
                "--cwd".into(),
                text(&self.plan["slots"][text(&self.plan, "root_slot")?], "cwd")?.into(),
                "--label".into(),
                text(&self.record, "label")?.into(),
                "--no-focus".into(),
            ],
            "split" => {
                self.validate_bound()?;
                let slot = slot.context("missing split slot")?;
                let step = self.plan["splits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["slot"] == slot)
                    .context("unknown split slot")?;
                vec![
                    "pane".into(),
                    "split".into(),
                    text(&self.record["bindings"][text(step, "parent")?], "pane_id")?.into(),
                    "--direction".into(),
                    text(step, "direction")?.into(),
                    "--ratio".into(),
                    format!("{:.2}", step["percent"].as_f64().unwrap() / 100.0),
                    "--cwd".into(),
                    text(&self.plan["slots"][slot], "cwd")?.into(),
                    "--no-focus".into(),
                ]
            }
            "tag_workspace" | "tag_pane" => {
                let (group, id) = if kind == "tag_workspace" {
                    self.workspace(false)?;
                    ("workspace", text(&self.record, "workspace_id")?)
                } else {
                    self.workspace(true)?;
                    let slot = slot.context("missing tag slot")?;
                    ensure!(
                        self.binding_visible(slot, false)?,
                        "cannot tag a changed pane"
                    );
                    ("pane", text(&self.record["bindings"][slot], "pane_id")?)
                };
                let mut args = vec![
                    group.into(),
                    "report-metadata".into(),
                    id.into(),
                    "--source".into(),
                    "ccb".into(),
                    "--token".into(),
                    format!("ccb_native_owner={}", text(&self.record, "owner_id")?),
                    "--token".into(),
                    format!("ccb_project_id={}", text(&self.plan, "project_id")?),
                    "--token".into(),
                    format!("ccb_namespace_id={}", text(&self.record, "workspace_id")?),
                ];
                if let Some(slot) = slot {
                    args.extend([
                        "--title".into(),
                        slot.into(),
                        "--token".into(),
                        format!("ccb_slot={slot}"),
                    ]);
                }
                args
            }
            _ => anyhow::bail!("unknown namespace mutation"),
        };
        let environment_slot = match kind {
            "create_workspace" => Some(text(&self.plan, "root_slot")?),
            "split" => slot,
            _ => None,
        };
        if let Some(slot) = environment_slot {
            for (key, value) in launch_env(self.request, slot)? {
                args.extend(["--env".into(), format!("{key}={}", value.as_str().unwrap())]);
            }
        }
        self.record["pending"]["sent"] = json!(true);
        self.save()?;
        let result = self.rpc(args)?;
        match kind {
            "create_workspace" => {
                let id = text(&result["workspace"], "workspace_id")?;
                let pane = text(&result["root_pane"], "pane_id")?;
                ensure!(
                    result["root_pane"]["workspace_id"] == id
                        && !pending["baseline"].as_array().unwrap().contains(&json!(id)),
                    "workspace creation returned an existing or foreign workspace"
                );
                self.record["workspace_id"] = json!(id);
                self.record["bindings"][text(&self.plan, "root_slot")?] =
                    json!({"pane_id":pane,"tagged":false});
            }
            "split" => {
                let pane = text(&result["pane"], "pane_id")?;
                ensure!(
                    result["pane"]["workspace_id"] == self.record["workspace_id"]
                        && !pending["baseline"]
                            .as_array()
                            .unwrap()
                            .contains(&json!(pane)),
                    "split returned an existing or foreign pane"
                );
                self.record["bindings"][slot.unwrap()] = json!({"pane_id":pane,"tagged":false});
            }
            "tag_workspace" => {
                self.workspace(true)?;
                self.record["workspace_tagged"] = json!(true);
            }
            "tag_pane" => {
                ensure!(
                    self.binding_visible(slot.unwrap(), true)?,
                    "pane ownership metadata is not visible"
                );
                self.record["bindings"][slot.unwrap()]["tagged"] = json!(true);
            }
            _ => unreachable!(),
        }
        self.record["pending"] = Value::Null;
        self.save()
    }
    fn prepare(&mut self) -> Result<Value> {
        loop {
            if !self.record["pending"].is_null() {
                if self.record["pending"]["sent"] == true {
                    if !self.observe_pending()? {
                        return self.uncertain();
                    }
                } else {
                    self.apply_pending()?;
                }
            } else if !self.next()? {
                return Ok(self.record.clone());
            }
        }
    }
    fn confirm(&mut self) -> Result<()> {
        let pending = self.record["pending"].clone();
        ensure!(
            !pending.is_null()
                && self.request["confirm_pending_creation"] == true
                && self.request["operation_id"] == pending["operation_id"],
            "confirmation requires the exact pending operation identity"
        );
        match text(&pending, "kind")? {
            "create_workspace" => {
                let id = text(self.request, "workspace_id")?;
                let pane = text(self.request, "pane_id")?;
                ensure!(
                    !pending["baseline"].as_array().unwrap().contains(&json!(id)),
                    "cannot adopt a pre-existing workspace"
                );
                ensure!(
                    self.workspaces()?
                        .iter()
                        .filter(|w| w["workspace_id"] == id
                            && w["label"] == self.record["label"]
                            && w["tokens"]["ccb_native_owner"].is_null())
                        .count()
                        == 1,
                    "confirmed workspace is not the unowned creation target"
                );
                let panes: Vec<Value> = serde_json::from_value(
                    self.rpc(vec![
                        "pane".into(),
                        "list".into(),
                        "--workspace".into(),
                        id.into(),
                    ])?["panes"]
                        .clone(),
                )
                .context("invalid confirmed pane inventory")?;
                ensure!(
                    panes.len() == 1
                        && panes[0]["pane_id"] == pane
                        && panes[0]["workspace_id"] == id,
                    "confirmed workspace must contain its single root pane"
                );
                self.record["workspace_id"] = json!(id);
                self.record["bindings"][text(&self.plan, "root_slot")?] =
                    json!({"pane_id":pane,"tagged":false});
            }
            "split" => {
                self.workspace(true)?;
                let id = text(self.request, "pane_id")?;
                ensure!(
                    !pending["baseline"].as_array().unwrap().contains(&json!(id)),
                    "cannot adopt a pre-existing pane"
                );
                ensure!(
                    self.panes()?
                        .iter()
                        .filter(|p| p["pane_id"] == id
                            && p["workspace_id"] == self.record["workspace_id"]
                            && p["tokens"]["ccb_native_owner"].is_null())
                        .count()
                        == 1,
                    "confirmed split is absent or already owned"
                );
                self.record["bindings"][text(&pending, "slot")?] =
                    json!({"pane_id":id,"tagged":false});
            }
            "tag_workspace" | "tag_pane" => {
                // An explicit operator confirmation permits reapplying the same
                // metadata to the recorded target; normal prepare never does.
                self.record["pending"]["sent"] = json!(false);
                return self.save();
            }
            _ => anyhow::bail!("unknown namespace confirmation"),
        }
        self.record["pending"] = Value::Null;
        self.save()
    }
}

pub(crate) fn with_prepared(
    request: &Value,
    action: impl FnOnce(&mut Store, &Value, &Value) -> Result<Value>,
) -> Result<Value> {
    let plan = plan(request)?;
    let signature = hash(&plan)?;
    let runtime = crate::config::state_root(Path::new(text(&plan, "project_root")?))?;
    let state = runtime.join("herdr");
    unlinked(&runtime)?;
    unlinked(&state)?;
    ensure!(
        state.join(RECORD).is_file(),
        "prepare the native Herdr namespace first"
    );
    let store = Store::open(&state)?;
    let record = store
        .json(RECORD)?
        .context("native Herdr namespace missing")?;
    validate_record(&record, &plan, &signature)?;
    ensure!(
        record["status"] == "prepared",
        "Herdr namespace preparation is incomplete"
    );
    let mut controller = Controller {
        store,
        request,
        plan,
        record,
    };
    controller.validate_bound()?;
    action(&mut controller.store, &controller.plan, &controller.record)
}
pub(crate) fn inspect_prepared(request: &Value) -> Result<Value> {
    inspect_status(request, "prepared")
}
pub(crate) fn inspect_retired(request: &Value) -> Result<Value> {
    inspect_status(request, "retired")
}
fn inspect_status(request: &Value, expected: &str) -> Result<Value> {
    let mut request = request.clone();
    request["action"] = json!("inspect");
    let namespace = execute(&request)?["namespace"].clone();
    let plan = plan(&request)?;
    validate_record(&namespace, &plan, &hash(&plan)?)?;
    ensure!(
        namespace["status"] == expected,
        "Herdr namespace is not {expected}"
    );
    Ok(namespace)
}
fn pre_backend_authority(runtime: &Path, plan: &Value) -> Result<crate::Kernel> {
    crate::daemon::verify_owner(runtime)?;
    ensure!(
        crate::keeper::status(runtime)?["alive"] == false,
        "native Keeper is still active"
    );
    let mut kernel = crate::Kernel::open(runtime)?;
    for path in [
        "configuration.json",
        "lease.json",
        "kernel/native-start.json",
        "kernel/native-keeper.json",
    ] {
        ensure!(
            crate::startup::load(&runtime.join(path))?.is_none(),
            "backend has prior ownership records; use committed recovery"
        );
    }
    for name in plan["slots"]
        .as_object()
        .context("namespace slots missing")?
        .keys()
    {
        ensure!(
            crate::records::dispatch(
                &mut kernel.store,
                "latest",
                &json!({"stream":"jobs","identity":"job_id","agent_name":name})
            )?
            .as_array()
            .is_some_and(|rows| rows.is_empty()),
            "pre-backend namespace has durable jobs; explicit recovery required"
        );
    }
    ensure!(
        crate::records::dispatch(
            &mut kernel.store,
            "latest",
            &json!({"stream":"callbacks","identity":"edge_id"})
        )?
        .as_array()
        .is_some_and(|rows| rows.is_empty()),
        "pre-backend namespace has callback records"
    );
    Ok(kernel)
}
fn retire_layout(
    request: &Value,
    plan: Value,
    signature: String,
    pre_backend: bool,
) -> Result<Value> {
    let project = Path::new(text(&plan, "project_root")?);
    let runtime = crate::config::state_root(project)?;
    let state = runtime.join("herdr");
    crate::daemon::verify_owner(&runtime)?;
    ensure!(
        crate::keeper::status(&runtime)?["alive"] == false,
        "stop the native Keeper before namespace retirement"
    );
    std::fs::create_dir_all(runtime.join("kernel"))?;
    let mut locks = Vec::new();
    for path in [
        state.join("native-open-launch.lock"),
        state.join("native-open.lock"),
        runtime.join("kernel/native-keeper-launch.lock"),
        runtime.join("kernel/native-launch.lock"),
    ] {
        unlinked(&path)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock().context("Herdr open is still in progress")?;
        locks.push(file);
    }
    crate::daemon::verify_owner(&runtime)?;
    ensure!(
        crate::keeper::status(&runtime)?["alive"] == false,
        "native Keeper is still active"
    );
    let _initial = if pre_backend {
        Some(pre_backend_authority(&runtime, &plan)?)
    } else {
        None
    };
    let _authority = if pre_backend {
        None
    } else {
        Some(Store::open(&runtime)?)
    };
    crate::daemon::verify_owner(&runtime)?;
    let store = Store::open(&state)?;
    let record = store
        .json(RECORD)?
        .context("namespace disappeared during retirement")?;
    validate_record(&record, &plan, &signature)?;
    Controller {
        store,
        request,
        plan,
        record,
    }
    .retire(pre_backend)
}
pub(crate) fn retire_initial(request: &Value) -> Result<Value> {
    let plan = plan(request)?;
    retire_layout(request, plan.clone(), hash(&plan)?, true)
}
pub(crate) fn reopen_pre_backend(request: &Value) -> Result<bool> {
    let plan = plan(request)?;
    let signature = hash(&plan)?;
    let root = Path::new(text(&plan, "project_root")?);
    let runtime = crate::config::state_root(root)?;
    let state = runtime.join("herdr");
    let Some(record) = crate::startup::load(&state.join(RECORD))? else {
        return Ok(false);
    };
    if record["status"] != "retired" || record["retirement"]["pre_backend"] != true {
        return Ok(false);
    }
    let _authority = pre_backend_authority(&runtime, &plan)?;
    let store = Store::open(&state)?;
    let current = store
        .json(RECORD)?
        .context("retired namespace disappeared")?;
    ensure!(current == record, "retired namespace changed");
    validate_record(&record, &plan, &signature)?;
    let controller = Controller {
        store,
        request,
        plan: plan.clone(),
        record: record.clone(),
    };
    ensure!(
        !controller
            .workspaces()?
            .iter()
            .any(|w| w["workspace_id"] == record["workspace_id"]),
        "retired workspace identity reappeared"
    );
    let mut store = controller.store;
    let owner = text(&record, "owner_id")?;
    uuid::Uuid::parse_str(owner)?;
    store.save(&format!("archived-partial/{owner}/namespace.json"), &record)?;
    if let Some(agents) = store.json("herdr-agents.json")? {
        ensure!(
            agents["namespace_owner"] == record["owner_id"],
            "partial agent ledger belongs to another namespace"
        );
        for row in agents["agents"]
            .as_object()
            .context("partial agent rows missing")?
            .values()
        {
            for process in [
                &row["binding"]["process"],
                &row["host_launch"]["host_process"],
                &row["host_launch"]["server_process"],
            ] {
                if !process.is_null() {
                    ensure!(!crate::startup::owner_alive(&json!({"pid":process["pid"],"process":process}))?,
                        "closed workspace provider is still live or unconfirmed; wait before reopening");
                }
            }
        }
        store.save(&format!("archived-partial/{owner}/agents.json"), &agents)?;
        store.remove("herdr-agents.json")?;
    }
    store.remove(RECORD)?;
    store.remove("open-configuration.json")?;
    store.commit()?;
    Ok(true)
}
pub fn execute(request: &Value) -> Result<Value> {
    let plan = plan(request)?;
    let signature = hash(&plan)?;
    let action = text(request, "action")?;
    if action == "plan" {
        return Ok(json!({"plan":plan,"plan_hash":signature,"mutations":false}));
    }
    ensure!(
        ["prepare", "inspect", "confirm", "retire"].contains(&action),
        "unsupported namespace action"
    );
    let root = Path::new(text(&plan, "project_root")?);
    let runtime = crate::config::state_root(root)?;
    unlinked(&runtime)?;
    let state = runtime.join("herdr");
    unlinked(&state)?;
    unlinked(&state.join(RECORD))?;
    if action == "retire" {
        if !state.join(RECORD).exists() {
            return Ok(json!({"status":"not_found","mutations":false}));
        }
        return retire_layout(request, plan, signature, false);
    }
    if action == "inspect" {
        let record = match std::fs::File::open(state.join(RECORD)) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= 2 * 1024 * 1024,
                    "namespace inspection record too large"
                );
                serde_json::from_slice::<Value>(&bytes)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            record.is_null()
                || record["project_id"] == plan["project_id"]
                    && record["session"] == plan["session"],
            "namespace inspection scope mismatch"
        );
        return Ok(
            json!({"matches_plan":record.is_null()||record["plan_hash"]==signature,"namespace":record,"live_checked":false}),
        );
    }
    for slot in plan["slots"].as_object().unwrap().values() {
        ensure!(
            Path::new(text(slot, "cwd")?).is_dir(),
            "namespace workspaces must be materialized first"
        );
    }
    let mut store = Store::open(&state)?;
    if action == "prepare" {
        crate::herdr_open::record_preparation(&mut store, request)?;
    }
    let record = if let Some(record) = store.json(RECORD)? {
        validate_record(&record, &plan, &signature)?;
        ensure!(
            !["retiring", "retired"].contains(&record["status"].as_str().unwrap_or("")),
            "namespace is retiring or retired; explicit lifecycle recovery is required"
        );
        record
    } else {
        ensure!(action == "prepare", "no namespace operation to confirm");
        let owner = uuid::Uuid::new_v4().to_string();
        let record = json!({"schema_version":1,"record_type":"native_herdr_namespace","project_id":plan["project_id"],"session":plan["session"],"owner_id":owner,"label":format!("ccb-native-{owner}"),"plan_hash":signature,"plan":plan,"workspace_id":null,"workspace_tagged":false,"bindings":{},"pending":null,"status":"preparing"});
        store.save(RECORD, &record)?;
        store.commit()?;
        record
    };
    let mut controller = Controller {
        store,
        request,
        plan,
        record,
    };
    ensure!(
        controller.rpc(vec!["api".into(), "schema".into(), "--json".into()])?["title"]
            == "Herdr API",
        "unsupported Herdr API schema"
    );
    if action == "confirm" {
        controller.confirm()?;
    }
    let result = controller.prepare();
    match result {
        Ok(record) => Ok(json!({"namespace":record,"provider_commands_sent":false})),
        Err(error) => {
            if error.downcast_ref::<Persistence>().is_some() {
                return Err(error);
            }
            if !controller.record["pending"].is_null() {
                Ok(json!({"namespace":controller.uncertain()?,"provider_commands_sent":false}))
            } else {
                Err(error)
            }
        }
    }
}
