//! Explicit opt-in integration: a real native service with no provider process.
use ccb_agent_sidebar::{args::Args, client::CcbdClient};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Service {
    child: Child,
    root: PathBuf,
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // This test creates a single unique direct child of the OS temp root.
        if let (Ok(path), Ok(parent)) = (
            self.root.canonicalize(),
            std::env::temp_dir().canonicalize(),
        ) && path.parent() == Some(parent.as_path())
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("ccb-sidebar-native-test-"))
        {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
fn command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}
fn cli(binary: &Path, root: &Path, args: &[&str]) -> Value {
    let output = command(binary)
        .arg("--state-root")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
#[ignore = "requires CCB_TEST_NATIVE_BIN and a built native core"]
fn authenticated_native_view_cancel_dismiss_stop_and_no_legacy_control_fallback() {
    let binary = PathBuf::from(
        std::env::var_os("CCB_TEST_NATIVE_BIN").expect("CCB_TEST_NATIVE_BIN is required"),
    );
    let root = std::env::temp_dir().join(format!(
        "ccb-sidebar-native-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    // Agent journals are siblings of the ccbd directory. Keep that entire
    // runtime parent inside this test's unique directory.
    let state = root.join("ccbd");
    let config = root.join("config.json");
    std::fs::write(&config,json!({"schema_version":2,"project_id":"sidebar-test","agents":{"worker":{"provider":"test","command":null}}}).to_string()).unwrap();
    let child = command(&binary)
        .arg("--state-root")
        .arg(&state)
        .args(["daemon", "--config"])
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut service = Service {
        child,
        root: root.clone(),
    };
    let mut ready = String::new();
    BufReader::new(service.child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert!(!ready.is_empty(), "native service exited before ready");
    let args = Args::parse_from([
        "--state-root".to_string(),
        state.display().to_string(),
        "--native-bin".into(),
        binary.display().to_string(),
        "--dump-json".into(),
    ])
    .unwrap();
    let client = CcbdClient::from_args(&args).unwrap();
    assert!(client.is_native());
    assert_eq!(client.project_view().unwrap().view.agents[0].name, "worker");
    let submitted = cli(
        &binary,
        &state,
        &["ask", "--from", "user", "worker", "native sidebar task"],
    );
    let id = submitted["job_id"].as_str().unwrap();
    assert!(client.dismiss_comms(id).is_err());
    assert!(client.focus_agent("worker", Some(1)).is_err());
    assert!(client.restart_panes().is_err());
    assert!(client.reload_config().is_err());
    client.cancel(id).unwrap();
    let response = client.project_view().unwrap();
    assert_eq!(response.view.comms[0].status, "cancelled");
    assert_eq!(response.view.comms[0].cancel_enabled, Some(false));
    client.dismiss_comms(id).unwrap();
    assert!(client.project_view().unwrap().view.comms.is_empty());
    assert_eq!(cli(&binary, &state, &["watch", id])["status"], "cancelled");
    client.stop_native().unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while service.child.try_wait().unwrap().is_none() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        service.child.try_wait().unwrap().is_some(),
        "native stop must terminate only its selected service"
    );
}
