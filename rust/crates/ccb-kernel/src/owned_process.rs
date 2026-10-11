//! A spawned process remains owned even when subsequent transport setup fails.
use anyhow::Result;
use std::{
    ops::{Deref, DerefMut},
    process::{Child, Command},
};

pub(crate) struct OwnedChild(Child);
impl OwnedChild {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        Ok(Self(command.spawn()?))
    }
}
impl Deref for OwnedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
