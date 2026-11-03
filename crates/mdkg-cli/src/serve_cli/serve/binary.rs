//! The program serve runs for each request: its own build, checked
//! before every spawn (orchestrator #12136, decision 2 of #12135).
//!
//! serve is long-running and runs the CLI as a child for each request (an
//! event stream's `msg watch`, each operation). A reinstall while it runs
//! would make those children a different build than serve: a newer one
//! can upgrade the store under the running serve, an older one is the
//! build AGENTS.md says must never run there again. So serve fingerprints
//! its binary at start (mdkg-run's `BinaryWatch`, as `agents up` does),
//! and checks it before each spawn and on each tick of its run loop. Once
//! it has changed, a request is refused with `api.draining` and serve
//! drains, so its restart runs the new build; an idle serve drains
//! within a tick. Every later spawn is refused too, whatever the file
//! does next. Off Linux a child runs the file by its path, so one renamed
//! in between a check and the exec runs as the new build once; the next
//! check drains serve (a stated residual).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::ErrorKind;
use mdkg_run::builds::BinaryWatch;
use mdkg_wire::codec::Response;

use super::local::refusal;

const REPLACED: &str = "serve's program was replaced; serve is stopping to run the new build";

/// serve's own program, as started.
#[derive(Debug)]
pub struct Binary {
    /// What a child runs: `/proc/self/exe` on Linux, the canonical path of
    /// `current_exe()` elsewhere (serve-ops' `self_exe`, so the events
    /// route and the op routes run the same file).
    exe: PathBuf,
    /// `None` only for a test's program that never changes.
    watch: Option<BinaryWatch>,
    /// Latched once a check sees a change.
    changed: AtomicBool,
}

impl Binary {
    /// This process's program, fingerprinted now: the earlier, the less
    /// a replacement before it goes unseen.
    pub fn at_start() -> std::io::Result<Self> {
        let exe = crate::ops_exec::self_exe()?;
        let watch = mdkg_run::supervise::reload::binary_at_start()
            .ok_or_else(|| std::io::Error::other("serve's own program can't be fingerprinted"))?;
        Ok(Self::with(exe, Some(watch)))
    }

    /// `exe`, fingerprinted with `invocation` as the path it was started
    /// by.
    pub fn new(exe: &Path, invocation: &Path) -> std::io::Result<Self> {
        Ok(Self::with(
            exe.to_path_buf(),
            Some(BinaryWatch::new(exe, invocation)?),
        ))
    }

    /// `exe`, never checked: a test's program.
    #[cfg(test)]
    pub fn fixed(exe: &Path) -> Self {
        Self::with(exe.to_path_buf(), None)
    }

    fn with(exe: PathBuf, watch: Option<BinaryWatch>) -> Self {
        Self {
            exe,
            watch,
            changed: AtomicBool::new(false),
        }
    }

    /// Whether serve's program was replaced: the file checked now, and
    /// latched once a change is seen. Each spawn asks it, and so does
    /// serve's run loop on every tick, so an idle serve drains too
    /// (#12297, orchestrator #12299).
    pub fn replaced(&self) -> bool {
        if self.changed.load(Ordering::SeqCst) {
            return true;
        }
        let now = self.watch.as_ref().is_some_and(BinaryWatch::changed);
        if now {
            self.changed.store(true, Ordering::SeqCst);
        }
        now
    }

    /// serve-ops' pre-spawn check for the op routes
    /// (`SubprocessBackend::with_check`): the same check as
    /// [`Binary::program`], one function for both (orchestrator #12136).
    pub fn check(&self) -> Result<(), crate::ops_exec::served::Refused> {
        if self.replaced() {
            return Err(crate::ops_exec::served::Refused {
                code: Code::ApiDraining,
                kind: ErrorKind::Failure,
                message: REPLACED.into(),
            });
        }
        Ok(())
    }

    /// The program to spawn, or, once it was replaced, the `api.draining`
    /// refusal.
    pub fn program(&self) -> Result<&Path, Response> {
        if self.replaced() {
            // api.draining's status is 503 (A12's exceptions).
            return Err(refusal(
                ErrorKind::Failure,
                Diagnostic::error(Code::ApiDraining, REPLACED)
                    .with_hint("retry once serve has restarted"),
            ));
        }
        Ok(&self.exe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_replaced_program_is_refused_from_then_on() {
        let dir = std::env::temp_dir().join(format!("mdkg-binary-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("dir");
        let exe = dir.join("mdkg");
        std::fs::write(&exe, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let b = Binary::new(&exe, &exe).expect("watch");
        assert_eq!(b.program().expect("same build"), exe.as_path());
        assert!(!b.replaced());
        // A reinstall: a new file renamed over the old one.
        let next = dir.join("mdkg.next");
        std::fs::write(&next, "#!/bin/sh\n# new\n").expect("write");
        std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)).expect("mode");
        std::fs::rename(&next, &exe).expect("rename");
        let r = b.program().expect_err("replaced");
        assert_eq!(r.status, 503);
        let v: serde_json::Value = serde_json::from_slice(&r.body).expect("json");
        assert_eq!(v["diagnostics"][0]["code"], "api.draining");
        assert!(b.replaced());
        // Latched: refused even if the old file came back.
        assert!(b.program().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
