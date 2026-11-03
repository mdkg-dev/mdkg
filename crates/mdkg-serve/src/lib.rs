//! `mdkg serve`: the door to one graph's coordinator store over a network
//! (the serve edd, lane L2).
//!
//! `serve` owns transport facts only (S19, R27): who signed a request, for
//! which graph and boot, how fresh it is, how large, and how often. Every
//! state decision is the kernel's, behind [`mdkg_ops::Backend`].
//!
//! The boundaries other lanes fill in are traits here: HTTP framing and TLS
//! (`mdkg-wire`), Ed25519 and person-signature verification, and the
//! store's tables (`serve.rs`) and event rendering. Nothing here opens the
//! store.

pub mod auth;
pub mod b64;
pub mod boot;
pub mod enroll;
pub mod http;
pub mod limits;
#[cfg(unix)]
pub mod listen;
pub mod nonce;
pub mod person_proof;
pub mod random;
pub mod refusal;
pub mod router;
pub mod sf;
#[cfg(unix)]
pub mod state;
pub mod tail;
pub mod verify;

#[cfg(test)]
mod guards {
    /// Every file under this crate, recursively.
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                files(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    #[test]
    fn only_the_gate_makes_a_verified() {
        // Terra 3 #10813, orchestrator #10750: `mdkg_ops::wire::Verified`
        // is what a remote Ctx is built from, and is non-exhaustive, so
        // outside mdkg-ops only its `new` makes one. Every call of it in
        // the workspace's crates must be `Authenticated::verified()`'s in
        // auth.rs, after the Gate's whole S5 pass. The name is assembled
        // so this file doesn't hold it.
        let call = concat!("Verified", "::new(");
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates");
        let gate = crates.join("mdkg-serve").join("src").join("auth.rs");
        let mut all = Vec::new();
        files(crates, &mut all);
        let mut callers = Vec::new();
        for path in all {
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for _ in text.matches(call) {
                callers.push(path.clone());
            }
        }
        assert_eq!(callers, vec![gate], "{call} outside the Gate");
    }

    #[test]
    fn serve_never_makes_a_direct_host_context() {
        // #10686 items 1 and 3: serve-store's DirectHost, the only context
        // in which a keyless host approval is allowed, is made by the
        // host's in-process command alone (the direct CLI and its stdio
        // MCP). Its constructor is `pub` so mdkg-cli can call it, so the
        // compiler can't keep this crate from calling it: this test does.
        // serve's request path passes no DirectHost. The name is assembled
        // so this file doesn't hold it.
        let name = concat!("in_process", "_host_command");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut all = Vec::new();
        files(root, &mut all);
        assert!(all.len() > 20, "the walk sees the crate");
        for path in all {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            assert!(!text.contains(name), "{} names {name}", path.display());
        }
    }
}
