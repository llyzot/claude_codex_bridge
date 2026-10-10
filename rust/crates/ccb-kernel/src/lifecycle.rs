//! Lease mutation and ownership policy, serialized with all other core state.
use crate::{parse_time, records::string, Kernel};
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};

fn number(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}
fn validate(kind: &str, record: &Value) -> Result<()> {
    ensure!(
        record["schema_version"] == 2 && record["record_type"] == kind,
        "invalid {kind} schema"
    );
    ensure!(
        !string(record, "project_id")?.trim().is_empty(),
        "empty project id"
    );
    if kind == "ccbd_lease" {
        ensure!(
            record["api_version"] == 2
                && number(record, "ccbd_pid") > 0
                && number(record, "generation") > 0,
            "invalid lease identity"
        );
        for key in ["socket_path", "boot_id", "started_at", "last_heartbeat_at"] {
            ensure!(!string(record, key)?.is_empty(), "empty lease {key}");
        }
        ensure!(
            ["mounted", "unmounted"].contains(&string(record, "mount_state")?),
            "invalid mount state"
        );
    } else {
        ensure!(
            ["running", "stopped"].contains(&string(record, "desired_state")?),
            "invalid desired state"
        );
        ensure!(
            ["unmounted", "starting", "mounted", "stopping", "failed"]
                .contains(&string(record, "phase")?),
            "invalid lifecycle phase"
        );
        ensure!(
            !string(record, "phase_started_at")?.trim().is_empty()
                && record["generation"].as_u64().is_some(),
            "invalid lifecycle generation"
        );
        for key in ["owner_pid", "keeper_pid", "socket_inode"] {
            ensure!(
                record[key].is_null() || number(record, key) > 0,
                "invalid lifecycle {key}"
            );
        }
        ensure!(
            record["namespace_epoch"].is_null() || record["namespace_epoch"].as_u64().is_some(),
            "invalid namespace epoch"
        );
    }
    Ok(())
}
pub fn dispatch(kernel: &mut Kernel, op: &str, args: &Value) -> Result<Value> {
    let lifecycle = op.starts_with("lifecycle_");
    let path = if lifecycle {
        "lifecycle.json"
    } else {
        "lease.json"
    };
    if op == "load" || op == "lifecycle_load" {
        return Ok(kernel.store.json(path)?.unwrap_or(Value::Null));
    }
    if op == "save" || op == "lifecycle_save" {
        validate(
            if lifecycle {
                "ccbd_lifecycle"
            } else {
                "ccbd_lease"
            },
            &args["record"],
        )?;
        kernel.store.save(path, &args["record"])?;
        return Ok(Value::Null);
    }
    let mut lease = kernel.store.json("lease.json")?.unwrap_or(Value::Null);
    if lease.is_null() {
        if op == "release" {
            return Ok(Value::Null);
        }
        bail!("ccbd lease does not exist");
    }
    if op == "heartbeat" && lease["mount_state"] != "mounted" {
        return Ok(lease);
    }
    if op == "signature" {
        ensure!(
            lease["mount_state"] == "mounted",
            "ccbd lease is not mounted"
        );
    }
    if !args["expected_pid"].is_null() {
        ensure!(
            lease["ccbd_pid"] == args["expected_pid"],
            "ccbd lease holder changed: pid mismatch"
        );
    }
    let instance = args["expected_daemon_instance_id"]
        .as_str()
        .unwrap_or("")
        .trim();
    if !instance.is_empty() {
        ensure!(
            lease["daemon_instance_id"].as_str().unwrap_or("").trim() == instance,
            "ccbd lease holder changed: daemon_instance_id mismatch"
        );
    }
    let timestamp = string(args, "now")?;
    match op {
        "heartbeat" => {
            let interval = args["interval_s"].as_f64().unwrap_or(5.0);
            let elapsed = parse_time(timestamp)
                .zip(lease["last_heartbeat_at"].as_str().and_then(parse_time))
                .and_then(|(now, last)| (now - last).num_microseconds())
                .map(|n| n as f64 / 1_000_000.0);
            if interval > 0.0 && elapsed.is_some_and(|n| n >= 0.0 && n < interval) {
                return Ok(lease);
            }
        }
        "signature" => {
            ensure!(
                lease["generation"] == args["expected_generation"],
                "ccbd lease generation changed"
            );
            let signature = string(args, "config_signature")?.trim();
            ensure!(!signature.is_empty(), "config_signature cannot be empty");
            lease["config_signature"] = json!(signature);
        }
        "release" => lease["mount_state"] = json!("unmounted"),
        _ => bail!("unsupported daemon lease operation"),
    }
    lease["last_heartbeat_at"] = json!(timestamp);
    validate("ccbd_lease", &lease)?;
    kernel.store.save("lease.json", &lease)?;
    Ok(lease)
}
pub fn ownership(args: &Value) -> Result<Value> {
    let lease = &args["lease"];
    if lease.is_null() {
        return Ok(
            json!({"health":"missing","pid_alive":false,"socket_connectable":false,"heartbeat_fresh":false,"takeover_allowed":true,"reason":"lease_missing"}),
        );
    }
    let alive = args["pid_alive"] == true;
    let socket = args["socket_connectable"] == true;
    let grace = args["heartbeat_grace_seconds"].as_f64().unwrap_or(15.0);
    let fresh = string(args, "now")
        .ok()
        .and_then(parse_time)
        .zip(lease["last_heartbeat_at"].as_str().and_then(parse_time))
        .and_then(|(now, last)| (now - last).num_microseconds())
        .is_some_and(|n| n as f64 / 1_000_000.0 <= grace);
    let unmounted = lease["mount_state"] == "unmounted";
    let takeover = unmounted || !alive || !fresh && !socket;
    let health = if unmounted {
        "unmounted"
    } else if alive && fresh && socket {
        "healthy"
    } else if takeover {
        "stale"
    } else {
        "degraded"
    };
    let mut reasons = Vec::new();
    if !alive {
        reasons.push("pid_missing");
    }
    if !fresh {
        reasons.push("heartbeat_stale");
    }
    if !socket {
        reasons.push("socket_unreachable");
    }
    Ok(
        json!({"health":health,"pid_alive":alive,"socket_connectable":socket,"heartbeat_fresh":fresh,"takeover_allowed":takeover,
        "reason":if unmounted {"lease_unmounted".into()} else if health=="healthy" {"healthy".into()} else {reasons.join(",")}}),
    )
}

pub fn claim(args: &Value, expected: bool) -> Result<Value> {
    let lease = &args["lease"];
    let generation = args["expected_generation"].as_u64().unwrap_or(0);
    if expected {
        ensure!(generation > 0, "expected ccbd generation must be positive");
    }
    if lease.is_null() {
        return Ok(if expected { Value::Null } else { json!(1) });
    }
    let current = number(lease, "generation");
    let same_project = lease["project_id"] == args["project_id"];
    let same_holder =
        lease["ccbd_pid"] == args["pid"] && lease["socket_path"] == args["socket_path"];
    if expected {
        if same_project
            && current == generation
            && same_holder
            && lease["daemon_instance_id"] == args["daemon_instance_id"]
        {
            return Ok(Value::Null);
        }
        if !same_project {
            ensure!(
                lease["mount_state"] == "unmounted" && current < generation,
                "ccbd lease project_id mismatch for expected claim"
            );
            return Ok(Value::Null);
        }
        ensure!(
            current < generation,
            "ccbd expected generation is already held or superseded"
        );
        ensure!(
            args["inspection"]["takeover_allowed"] == true,
            "ccbd predecessor lease does not allow expected claim"
        );
        return Ok(Value::Null);
    }
    if !same_project && lease["mount_state"] == "unmounted" {
        return Ok(json!(current + 1));
    }
    ensure!(same_project, "ccbd lease project_id mismatch");
    if same_holder {
        return Ok(json!(current));
    }
    ensure!(
        args["inspection"]["takeover_allowed"] == true,
        "ccbd lease is held by another live owner"
    );
    Ok(json!(current + 1))
}
