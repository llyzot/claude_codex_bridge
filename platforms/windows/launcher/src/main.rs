use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Debug)]
struct PythonCommand {
    executable: OsString,
    prefix_args: Vec<OsString>,
}
fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(normalize_exit_code(code)),
        Err(message) => {
            eprintln!("ccb Windows launcher: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<i32, String> {
    let executable =
        env::current_exe().map_err(|error| format!("cannot resolve launcher path: {error}"))?;
    let bin_dir = executable
        .parent()
        .ok_or_else(|| "launcher has no parent directory".to_string())?;
    let install_root = bin_dir
        .parent()
        .ok_or_else(|| "launcher must live under <install-root>\\bin".to_string())?;
    if env::var_os("CCB_NATIVE_CLI").is_some_and(|value| value == "1") {
        let native = bin_dir.join("ccb-native.exe");
        if !native.is_file() {
            return Err(format!(
                "Native CLI selected but missing: {}",
                native.display()
            ));
        }
        let forwarded = native_arguments(&executable, env::args_os().skip(1).collect())?;
        let mut command = Command::new(native);
        command
            .args(forwarded)
            .env("CCB_INSTALL_PREFIX", install_root);
        suppress_child_console(&mut command);
        return command
            .status()
            .map(|status| status.code().unwrap_or(1))
            .map_err(|error| format!("Native CLI failed: {error}"));
    }
    let script = entry_script(&executable, install_root)?;

    if !script.is_file() {
        return Err(format!(
            "Python entrypoint is missing: {}",
            script.display()
        ));
    }

    let forwarded: Vec<OsString> = env::args_os().skip(1).collect();
    let mut failures = Vec::new();
    for python in python_candidates(install_root) {
        let mut command = Command::new(&python.executable);
        command.args(&python.prefix_args);
        command.arg(&script);
        command.args(&forwarded);
        command.env("CCB_WINDOWS_LAUNCHER", &executable);
        command.env("CCB_INSTALL_PREFIX", install_root);
        // Initial Windows plugin projection can exceed the shared 30s budget.
        // Keep an explicit upper bound and respect a user's override.
        if env::var_os("CCB_STARTUP_TRANSACTION_TIMEOUT_S").is_none() {
            command.env("CCB_STARTUP_TRANSACTION_TIMEOUT_S", "600");
        }
        suppress_child_console(&mut command);

        match command.status() {
            Ok(status) => return Ok(status.code().unwrap_or(1)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                failures.push(format!(
                    "{}: not found",
                    PathBuf::from(&python.executable).display()
                ));
            }
            Err(error) => {
                failures.push(format!(
                    "{}: {error}",
                    PathBuf::from(&python.executable).display()
                ));
            }
        }
    }

    Err(format!(
        "Python 3.10+ was not found. Checked: {}",
        failures.join(", ")
    ))
}
fn native_arguments(executable: &Path, mut args: Vec<OsString>) -> Result<Vec<OsString>, String> {
    let stem = executable
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("ccb")
        .to_ascii_lowercase();
    match stem.as_str() {
        "ccb"|"ccb-windows-launcher"=>Ok(args),
        "ask"=>{if args.len()<2 || args[0]!="--state-root" {return Err("Native ask requires --state-root <directory> before target arguments".into());}args.insert(2,OsString::from("ask"));Ok(args)},
        _=>Err(format!("Native CLI does not yet support launcher {stem}; do not select CCB_NATIVE_CLI for this command")),
    }
}

fn entry_script(executable: &Path, install_root: &Path) -> Result<PathBuf, String> {
    let stem = executable
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("ccb")
        .to_ascii_lowercase();
    let relative = match stem.as_str() {
        "ccb" | "ccb-windows-launcher" => PathBuf::from("platforms").join("windows").join("ccb.py"),
        "ask" => PathBuf::from("bin").join("ask.py"),
        "autonew" => PathBuf::from("bin").join("autonew.py"),
        "ctx-transfer" => PathBuf::from("bin").join("ctx-transfer.py"),
        other => return Err(format!("unsupported launcher name: {other}.exe")),
    };
    Ok(install_root.join(relative))
}

fn python_candidates(install_root: &Path) -> Vec<PythonCommand> {
    let mut candidates = Vec::new();
    if let Some(explicit) = env::var_os("CCB_PYTHON").filter(|value| !value.is_empty()) {
        candidates.push(PythonCommand {
            executable: explicit,
            prefix_args: Vec::new(),
        });
    }
    candidates.push(PythonCommand {
        executable: install_root
            .join(".venv")
            .join("Scripts")
            .join("python.exe")
            .into_os_string(),
        prefix_args: Vec::new(),
    });
    candidates.push(PythonCommand {
        executable: OsString::from("py"),
        prefix_args: vec![OsString::from("-3")],
    });
    candidates.push(PythonCommand {
        executable: OsString::from("python"),
        prefix_args: Vec::new(),
    });
    candidates
}

fn normalize_exit_code(code: i32) -> u8 {
    if (0..=255).contains(&code) {
        code as u8
    } else {
        1
    }
}

#[cfg(target_os = "windows")]
fn suppress_child_console(command: &mut Command) {
    use std::io::IsTerminal;
    use std::os::windows::process::CommandExt;
    // Interactive invocations must inherit their console for Herdr/ConPTY.
    // Hide only control commands started without a terminal.
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return;
    }
    // CREATE_NO_WINDOW: without it a console-subsystem child (python.exe)
    // spawned from a parent with no console is allocated a brand-new visible
    // console window that flashes and then closes when the child exits.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn suppress_child_console(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_ask_keeps_global_state_arguments_before_operation() {
        let args = native_arguments(
            Path::new("ask.exe"),
            vec!["--state-root".into(), "C:\\test".into(), "worker".into()],
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                OsString::from("--state-root"),
                "C:\\test".into(),
                "ask".into(),
                "worker".into()
            ]
        );
        assert!(native_arguments(Path::new("autonew.exe"), Vec::new()).is_err());
    }

    #[test]
    fn maps_launcher_names_to_python_entrypoints() {
        let root = Path::new(r"C:\Users\tester\AppData\Local\codex-dual");
        assert_eq!(
            entry_script(Path::new(r"C:\x\bin\ccb.exe"), root).unwrap(),
            root.join("platforms").join("windows").join("ccb.py")
        );
        assert_eq!(
            entry_script(Path::new(r"C:\x\bin\ask.exe"), root).unwrap(),
            root.join("bin").join("ask.py")
        );
    }

    #[test]
    fn rejects_unknown_launcher_name() {
        let error =
            entry_script(Path::new(r"C:\x\bin\unknown.exe"), Path::new(r"C:\x")).unwrap_err();
        assert!(error.contains("unsupported launcher name"));
    }
}
