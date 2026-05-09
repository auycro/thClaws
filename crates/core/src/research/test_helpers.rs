//! Shared test scaffolding for the `research` module's tests.
//!
//! `kms_writer::tests` and `pipeline::tests` both touch `HOME` /
//! `USERPROFILE` / cwd to run against a fresh tempdir. Each module
//! having its own process-wide `Mutex` doesn't help when the tests
//! are scheduled in parallel across both modules — they race. This
//! file owns the *one* shared lock + RAII guard everyone uses.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn shared_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Acquire exclusive access to the process env + cwd, set HOME to a
/// fresh tempdir, switch cwd into it. Restored on drop.
pub(crate) fn scoped_home() -> ScopedHome {
    let guard = shared_lock().lock().unwrap_or_else(|p| p.into_inner());
    let prev_home = std::env::var("HOME").ok();
    let prev_userprofile = std::env::var("USERPROFILE").ok();
    let prev_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    std::env::set_var("USERPROFILE", dir.path());
    std::env::set_current_dir(dir.path()).unwrap();
    ScopedHome {
        _lock: guard,
        prev_home,
        prev_userprofile,
        prev_cwd,
        _home_dir: dir,
    }
}

pub(crate) struct ScopedHome {
    _lock: MutexGuard<'static, ()>,
    prev_home: Option<String>,
    prev_userprofile: Option<String>,
    prev_cwd: PathBuf,
    _home_dir: tempfile::TempDir,
}

impl Drop for ScopedHome {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev_cwd);
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match &self.prev_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
    }
}
