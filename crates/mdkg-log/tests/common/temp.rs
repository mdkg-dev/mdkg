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

// A separate counter lets the regression tests force a stale-name collision.
pub(crate) fn create_in(parent: &Path, prefix: &str, next: &AtomicU64) -> io::Result<PathBuf> {
    loop {
        let n = next.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{prefix}-{}-{n}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}
