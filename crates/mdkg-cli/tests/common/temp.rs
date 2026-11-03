//! Reserve test fixture roots without relying on clock resolution.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The caller owns this newly created directory and is responsible for cleanup.
pub fn dir(prefix: &str) -> PathBuf {
    create_in(&std::env::temp_dir(), prefix, &NEXT).expect("reserve test directory")
}

/// [`dir`], created so only this user can read or change it: for a build
/// whose result is kept and reused (a fixture template), which nobody else
/// may add to while it runs.
#[allow(dead_code)] // Binaries that include this file alone don't build templates.
pub fn private(prefix: &str) -> PathBuf {
    create_mode(&std::env::temp_dir(), prefix, &NEXT, 0o700).expect("reserve private directory")
}

// A separate counter lets the regression tests force a stale-name collision.
pub(crate) fn create_in(parent: &Path, prefix: &str, next: &AtomicU64) -> io::Result<PathBuf> {
    create_mode(parent, prefix, next, 0o777)
}

fn create_mode(parent: &Path, prefix: &str, next: &AtomicU64, mode: u32) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    loop {
        let n = next.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{prefix}-{}-{n}", std::process::id()));
        match fs::DirBuilder::new().mode(mode).create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}
