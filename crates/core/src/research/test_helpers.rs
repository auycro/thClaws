//! Shared test scaffolding for the `research` module's tests.
//!
//! `kms_writer::tests` and `pipeline::tests` both touch `HOME` /
//! `USERPROFILE` / cwd to run against a fresh tempdir. We serialise
//! against `kms::test_env_lock` — the crate-wide env lock — instead
//! of a research-local one, so a parallel test in repl/prompts/etc
//! that reads `current_dir()` or `$HOME` (e.g. the system-prompt
//! builder) can't see our tempdir leak across its own measurement.
//! A local lock here only blocked siblings inside `research::*`.

use std::path::PathBuf;
use std::sync::MutexGuard;

/// Acquire exclusive access to the process env + cwd, set HOME to a
/// fresh tempdir, switch cwd into it. Restored on drop.
pub(crate) fn scoped_home() -> ScopedHome {
    let guard = crate::kms::test_env_lock();
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
