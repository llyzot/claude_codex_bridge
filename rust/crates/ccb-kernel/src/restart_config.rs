//! User-bound recovery configuration committed with the native configuration
//! identity. Secrets never enter the state journal as plaintext.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

const LIMIT: usize = 1024 * 1024;
const CIPHER_LIMIT: usize = LIMIT + 16384;

fn entropy(project: &Value, fingerprint: &Value) -> Result<Vec<u8>> {
    ensure!(
        project.as_str().is_some_and(|p| !p.is_empty()),
        "restart project identity missing"
    );
    ensure!(
        fingerprint
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "restart configuration fingerprint invalid"
    );
    Ok(serde_json::to_vec(&json!([
        "ccb-native-restart-v1",
        project,
        fingerprint
    ]))?)
}

#[cfg(windows)]
fn crypt(input: &[u8], context: &[u8], protect: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::{Foundation::LocalFree, Security::Cryptography::*};
    struct Output(CRYPT_INTEGER_BLOB);
    impl Drop for Output {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                unsafe {
                    std::ptr::write_bytes(self.0.pbData, 0, self.0.cbData as usize);
                    LocalFree(self.0.pbData.cast());
                }
            }
        }
    }
    let data = CRYPT_INTEGER_BLOB {
        cbData: input.len().try_into()?,
        pbData: input.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: context.len().try_into()?,
        pbData: context.as_ptr().cast_mut(),
    };
    let mut output = Output(CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    });
    let success = unsafe {
        if protect {
            CryptProtectData(
                &data,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        } else {
            CryptUnprotectData(
                &data,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        }
    };
    ensure!(
        success != 0,
        "Windows could not protect or restore this user's restart configuration"
    );
    ensure!(
        !output.0.pbData.is_null()
            && output.0.cbData as usize <= if protect { CIPHER_LIMIT } else { LIMIT },
        "protected restart configuration exceeds size bound"
    );
    Ok(unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) }.to_vec())
}
#[cfg(not(windows))]
fn crypt(_: &[u8], _: &[u8], _: bool) -> Result<Vec<u8>> {
    anyhow::bail!("protected restart configuration currently requires Windows")
}

pub(crate) fn seal(config: &Value) -> Result<Value> {
    if !cfg!(windows) {
        return Ok(Value::Null);
    }
    let mut config = config.clone();
    config
        .as_object_mut()
        .context("configuration must be an object")?
        .remove("startup_id");
    let bytes = serde_json::to_vec(&config)?;
    ensure!(
        bytes.len() <= LIMIT,
        "restart configuration exceeds size bound"
    );
    let fingerprint = json!(crate::config::fingerprint(&config)?);
    let ciphertext = crypt(&bytes, &entropy(&config["project_id"], &fingerprint)?, true)?;
    Ok(
        json!({"schema_version":1,"protection":"windows-user-dpapi-v1",
        "ciphertext":ciphertext.iter().map(|b|format!("{b:02x}")).collect::<String>()}),
    )
}

fn restore(record: &Value) -> Result<Value> {
    ensure!(
        record["schema_version"] == 1 && record["record_type"] == "native_configuration",
        "invalid committed configuration identity"
    );
    let checkpoint = &record["restart_checkpoint"];
    ensure!(
        checkpoint["schema_version"] == 1 && checkpoint["protection"] == "windows-user-dpapi-v1",
        "no supported protected restart configuration; supply an explicit configuration"
    );
    let encoded = checkpoint["ciphertext"]
        .as_str()
        .context("protected restart configuration is missing")?;
    ensure!(
        !encoded.is_empty()
            && encoded.len() <= CIPHER_LIMIT * 2
            && encoded.len() % 2 == 0
            && encoded.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid protected restart configuration encoding"
    );
    let bytes = encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let clear = crypt(
        &bytes,
        &entropy(&record["project_id"], &record["config_fingerprint"])?,
        false,
    )?;
    let config: Value = serde_json::from_slice(&clear)
        .map_err(|_| anyhow::anyhow!("invalid protected restart configuration document"))?;
    ensure!(
        config["project_id"] == record["project_id"]
            && crate::config::fingerprint(&config)? == record["config_fingerprint"],
        "protected restart configuration identity mismatch"
    );
    crate::daemon::Daemon::validate_config(&config).map_err(|_| {
        anyhow::anyhow!("protected restart configuration is not supported by this runtime")
    })?;
    Ok(config)
}

pub(crate) fn load(root: &Path) -> Result<Value> {
    ensure!(
        cfg!(windows),
        "protected restart configuration currently requires Windows"
    );
    ensure!(
        root.join("configuration.json").is_file() || root.join("kernel/pending.json").is_file(),
        "no committed restart configuration at this state root"
    );
    crate::daemon::verify_owner(root)?;
    // A prior writer can die during commit. Its journal is authoritative and
    // must be replayed under the writer lock before selecting a configuration.
    let store = crate::store::Store::open(root)?;
    crate::daemon::verify_owner(root)?;
    let record = store
        .json("configuration.json")?
        .context("committed restart configuration missing")?;
    let lease = store
        .json("lease.json")?
        .context("restart configuration has no committed owner record")?;
    ensure!(
        lease["project_id"] == record["project_id"]
            && lease["config_signature"] == record["config_fingerprint"],
        "restart configuration and owner record disagree"
    );
    restore(&record)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    fn config() -> Value {
        json!({"schema_version":2,"project_id":"protected-test",
        "agents":{"worker":{"provider":"test","command":null,"env":{"SECRET":"private 中文 value"}}}})
    }
    fn record(config: &Value) -> Value {
        json!({"schema_version":1,"record_type":"native_configuration",
        "project_id":config["project_id"],"config_fingerprint":crate::config::fingerprint(config).unwrap(),
        "restart_checkpoint":seal(config).unwrap()})
    }
    #[test]
    fn protected_roundtrip_retains_values_without_boot_identity_or_plaintext() {
        let desired = config();
        let mut boot = desired.clone();
        boot["startup_id"] = json!("old-boot");
        let record = record(&boot);
        assert!(!record.to_string().contains("private"));
        assert_eq!(restore(&record).unwrap(), desired);
        assert_ne!(seal(&desired).unwrap(), seal(&desired).unwrap());
    }
    #[test]
    fn corrupt_or_rebound_ciphertext_cannot_supply_a_configuration() {
        let original = record(&config());
        for field in ["project_id", "config_fingerprint", "ciphertext", "encoding"] {
            let mut row = original.clone();
            match field {
                "project_id" => row[field] = json!("foreign-project"),
                "config_fingerprint" => row[field] = json!("0".repeat(64)),
                "ciphertext" => row["restart_checkpoint"][field] = json!("00".repeat(256)),
                _ => row["restart_checkpoint"]["ciphertext"] = json!("not-hex"),
            }
            assert!(restore(&row).is_err());
        }
    }
    #[test]
    fn interrupted_configuration_commit_restores_the_committed_pair() {
        for boundary in 0..=2 {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let mut store = crate::store::Store::open(root).unwrap();
            let first = config();
            let old = record(&first);
            let lease = |row: &Value| {
                json!({"schema_version":2,"record_type":"ccbd_lease",
                "project_id":row["project_id"],"mount_state":"unmounted","config_signature":row["config_fingerprint"]})
            };
            store.save("configuration.json", &old).unwrap();
            store.save("lease.json", &lease(&old)).unwrap();
            store.commit().unwrap();
            let mut desired = first.clone();
            desired["agents"]["worker"]["env"]["SECRET"] = json!("next-private-value");
            let next = record(&desired);
            store.save("configuration.json", &next).unwrap();
            store.save("lease.json", &lease(&next)).unwrap();
            assert_eq!(store.interrupt_for_test(boundary).unwrap(), 2);
            let ops: Vec<Value> =
                serde_json::from_slice(&std::fs::read(root.join("kernel/pending.json")).unwrap())
                    .unwrap();
            for op in ops {
                let bytes: Vec<u8> = serde_json::from_value(op["bytes"].clone()).unwrap();
                assert!(!String::from_utf8(bytes)
                    .unwrap()
                    .contains("next-private-value"));
            }
            drop(store);
            assert_eq!(load(root).unwrap(), desired);
            assert!(!root.join("kernel/pending.json").exists());
        }
    }
}
