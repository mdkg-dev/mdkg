//! A private temporary folder for tests, removed on drop.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let name = format!(
            "mdkg-wire-test-{}",
            crate::b64::encode_url(&crate::keys::random::<12>().expect("random"))
        );
        // Canonical, so paths compare equal to what git and the OS report
        // (macOS's /var is a link to /private/var).
        let base = fs::canonicalize(std::env::temp_dir()).expect("temp dir");
        let path = base.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("create temp dir");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
