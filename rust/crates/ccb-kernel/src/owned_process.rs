//! A spawned process remains owned even when subsequent transport setup fails.
#[cfg(not(windows))]
use anyhow::Result;
#[cfg(not(windows))]
use std::{
    ops::{Deref, DerefMut},
    process::{Child, Command},
};

#[cfg(windows)]
#[path = "windows_child.rs"]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{observe_external_process, probe_recovery, OwnedChild};

#[cfg(not(windows))]
pub(crate) struct OwnedChild(Child);
#[cfg(not(windows))]
impl OwnedChild {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        Ok(Self(command.spawn()?))
    }
    pub fn capture(command: &mut Command) -> Result<Self> {
        Self::spawn(command)
    }
    pub fn scope_info(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"supported":false}))
    }
    // Retain the prior Unix parent-process contract; tree control is unsupported.
    pub fn tree_empty(&self) -> std::io::Result<bool> {
        Ok(true)
    }
}
#[cfg(not(windows))]
impl Deref for OwnedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
#[cfg(not(windows))]
impl DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
#[cfg(not(windows))]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(not(windows))]
pub(crate) fn probe_recovery(_: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"status":"unavailable","reason":"platform_unsupported"})
}
#[cfg(not(windows))]
pub(crate) fn observe_external_process(_: u32) -> anyhow::Result<serde_json::Value> {
    anyhow::bail!("external process identity observation currently requires Windows")
}
