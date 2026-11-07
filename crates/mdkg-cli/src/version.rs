//! `mdkg version`: which build this is, so a reinstall can confirm the
//! binary it installed (orchestrator, 2026-10-22). The values are embedded
//! at build time by `build.rs`.

use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{self, Report};
use crate::schema::LEASE_AUTHORITY_CONTRACT;

/// Whether the build script's values are for this checkout: one that reused
/// another checkout's (a shared target folder) can't name the commit.
const HERE: bool = same(env!("MDKG_BUILD_ROOT"), env!("CARGO_MANIFEST_DIR"));

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `mdkg --version`'s line: the crate version, the commit, whether the
/// tree was dirty, and the build date; a build with no FIDO2 backend
/// (as mdkg-presence resolved its feature) says so.
pub const LINE: &str = match (HERE, mdkg_serve::FIDO2) {
    (true, true) => env!("MDKG_BUILD_VERSION"),
    (false, true) => env!("MDKG_BUILD_VERSION_ELSEWHERE"),
    (true, false) => env!("MDKG_BUILD_VERSION_NO_FIDO2"),
    (false, false) => env!("MDKG_BUILD_VERSION_ELSEWHERE_NO_FIDO2"),
};

#[derive(Debug, Serialize, JsonSchema)]
pub struct VersionData {
    /// Implemented API seams; clients check these rather than the build version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// The crate version.
    pub version: String,
    /// The full SHA of the commit this binary was built from, or `unknown`
    /// when it was built without git (from a tarball).
    pub commit: String,
    /// Whether that worktree had uncommitted changes to the tracked files
    /// that build the binary (`crates/`, the workspace manifest, lock and
    /// toolchain files); `null` when unknown.
    pub dirty: Option<bool>,
    /// When it was built, in UTC (RFC 3339): `$SOURCE_DATE_EPOCH` when the
    /// build set it.
    pub built: String,
    /// The lease-authority contract this build speaks, as its claim,
    /// release, renew and holds schemas carry it
    /// (`x-mdkg-lease-authority`).
    pub lease_authority: u64,
}

pub fn data() -> VersionData {
    VersionData {
        // A17: each is added when its feature ships. `events` and `msg.watch`
        // ship with the stream's signal ending (A18); `serve.local` with
        // `mdkg serve`'s socket (its start envelope names the same).
        capabilities: Some(vec![
            "agents.delivery.digest".into(),
            "channels".into(),
            "events".into(),
            "msg.watch".into(),
            "serve.local".into(),
        ]),
        version: env!("CARGO_PKG_VERSION").into(),
        commit: if HERE {
            env!("MDKG_BUILD_COMMIT")
        } else {
            "unknown"
        }
        .into(),
        dirty: match (HERE, env!("MDKG_BUILD_DIRTY")) {
            (true, "true") => Some(true),
            (true, "false") => Some(false),
            _ => None,
        },
        built: env!("MDKG_BUILD_DATE").into(),
        lease_authority: LEASE_AUTHORITY_CONTRACT,
    }
}

/// The same embedded identity used by `version`, for local process receipts.
pub fn build() -> mdkg_run::builds::Build {
    let data = data();
    mdkg_run::builds::Build {
        version: data.version,
        commit: data.commit,
        dirty: data.dirty,
        built: data.built,
    }
}

/// Keep a receipt locked for the caller's lifetime. Registration is diagnostic;
/// failure must not prevent an MCP session or an agent turn from running.
/// A missing store folder skips registration without initializing metadata.
pub fn hold_process(
    root: &std::path::Path,
    kind: mdkg_run::builds::Kind,
    agents: Vec<String>,
) -> Option<mdkg_run::builds::Held> {
    let publish = || -> Result<_, mdkg_run::RunError> {
        let dir = mdkg_run::state::run_dir(root)?;
        let executable = std::env::current_exe()
            .map_err(|e| mdkg_run::RunError::io("can't find the running executable", e))?;
        mdkg_run::builds::publish(&dir, kind, &executable, build(), agents)
    };
    match publish() {
        Ok(held) => held,
        Err(e) => {
            eprintln!(
                "warning: can't register process build: {}",
                output::shown_path(&e.message)
            );
            None
        }
    }
}

pub fn run(json: bool) -> u8 {
    output::emit("version", json, Report::ok(data()), |_| {
        println!("{} {LINE}", mdkg_format::program::BIN);
    })
}
