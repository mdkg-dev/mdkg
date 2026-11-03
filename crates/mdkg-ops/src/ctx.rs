//! The request context: everything an operation reads that today comes
//! from process-wide state (the serve edd's "The context").

use std::fmt;
use std::sync::{Arc, Mutex};

use mdkg_coord_sqlite::git::WorktreeTrust;
use mdkg_format::{Code, Diagnostic};

use crate::wire::{Backend, Verified};

/// The interface a request came in on: for the access log only, never for
/// policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Cli,
    Mcp,
    Http,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Cli => "cli",
            Transport::Mcp => "mcp",
            Transport::Http => "http",
        }
    }
}

/// The clock and the lease TTL. The CLI adapter reads the test clock and
/// the TTL variables once into this; `serve` sets its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub now_ms: u64,
    pub lease_ttl_ms: u64,
}

/// Notices collected for one operation, in place of the CLI's global list.
#[derive(Debug, Default)]
pub struct Notices(Mutex<Vec<Diagnostic>>);

impl Notices {
    pub fn push(&self, d: Diagnostic) {
        if let Ok(mut list) = self.0.lock() {
            list.push(d);
        }
    }

    /// Every notice so far, leaving none.
    pub fn take(&self) -> Vec<Diagnostic> {
        self.0
            .lock()
            .map(|mut list| std::mem::take(&mut *list))
            .unwrap_or_default()
    }
}

/// Actor name prefixes only mdkg itself writes as: `mdkg:` (`mdkg:serve`,
/// `mdkg:receipts`, …) and `launcher:`, whose actors are minted per
/// machine and never configured (S27).
pub const RESERVED_PREFIXES: &[&str] = &["mdkg:", "launcher:"];

/// Whether `actor` has a reserved prefix, in any case.
pub fn is_reserved(actor: &str) -> bool {
    RESERVED_PREFIXES.iter().any(|p| {
        actor
            .get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
    })
}

/// Where a request came from: trusted, because only the adapter that
/// received the request sets it, never request data. Kernel policy reads it,
/// for example to tell a person's action in the host's CLI from the same
/// request over the network (S20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// This host's own CLI, MCP server or launcher, opening the store.
    Local,
    /// Over the network, from the machine whose signature `serve`
    /// verified.
    Remote { machine: String },
}

/// The fields of a [`Ctx`] the adapter fills from its own input. Where the
/// request came from, its machine and its person are set by
/// [`Ctx::local`] or [`Ctx::remote`] instead.
#[derive(Debug)]
pub struct Parts {
    pub actor: String,
    pub agent: Option<String>,
    pub turn: Option<String>,
    pub transport: Transport,
    pub idempotency_key: Option<String>,
    pub clock: Clock,
    pub trust: WorktreeTrust,
    pub backend: Arc<dyn Backend>,
}

impl fmt::Debug for dyn Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.kind())
    }
}

/// One request's context. A request's `apply` runs on one thread with it.
pub struct Ctx {
    origin: Origin,
    machine: String,
    person: Option<String>,
    pub actor: String,
    /// For a launcher step, the agent it's for. A recovery step binds to
    /// the machine and this agent, never to the supervisor process that
    /// sent it, so a restarted supervisor can finish it (S10).
    pub agent: Option<String>,
    /// The acting turn, from `MDKG_RUN_TURN` locally or `Mdkg-Turn`
    /// remotely. It binds the request to the turn's agent and machine
    /// (S20); its absence grants nothing.
    pub turn: Option<String>,
    /// For the access log only. Policy reads [`Ctx::origin`].
    pub transport: Transport,
    pub idempotency_key: Option<String>,
    pub clock: Clock,
    /// The worktree trust pins, scoped with `with_worktree_trust`.
    pub trust: WorktreeTrust,
    pub notices: Notices,
    /// Where `apply` runs: this process's store, or a coordinator.
    pub backend: Arc<dyn Backend>,
}

impl Ctx {
    /// A request this host's own CLI, MCP server or launcher makes: as the
    /// host's machine, and as the person whose key signed it on the host,
    /// if one did (S41): the key id and its `human:*` person. Refused with
    /// `api.actor_not_allowed` when `parts.actor` isn't that person.
    ///
    /// Only the host's own adapters call it: `local_is_never_built_off_the_host`
    /// keeps it out of every other crate.
    pub fn local(
        host_machine: String,
        person: Option<(String, String)>,
        parts: Parts,
    ) -> Result<Self, Box<Diagnostic>> {
        Self::new(Origin::Local, host_machine, person, parts)
    }

    /// A request `serve` received and verified: its origin, machine and
    /// person come from the verification alone, whatever the request says.
    /// Refused with `api.actor_not_allowed` when a person signed and
    /// `parts.actor` isn't that person (SC68).
    pub fn remote(verified: &Verified, parts: Parts) -> Result<Self, Box<Diagnostic>> {
        Self::new(
            Origin::Remote {
                machine: verified.machine().to_owned(),
            },
            verified.machine().to_owned(),
            verified.person().map(|(k, h)| (k.to_owned(), h.to_owned())),
            parts,
        )
    }

    fn new(
        origin: Origin,
        machine: String,
        person: Option<(String, String)>,
        p: Parts,
    ) -> Result<Self, Box<Diagnostic>> {
        // A person signature acts only as its own person: never as the
        // actor a request names.
        if let Some((_, human)) = &person
            && *human != p.actor
        {
            return Err(Box::new(Diagnostic::error(
                Code::ApiActorNotAllowed,
                "the request's actor isn't the person whose key signed it",
            )));
        }
        Ok(Self {
            origin,
            machine,
            person: person.map(|(key, _)| key),
            actor: p.actor,
            agent: p.agent,
            turn: p.turn,
            transport: p.transport,
            idempotency_key: p.idempotency_key,
            clock: p.clock,
            trust: p.trust,
            notices: Notices::default(),
            backend: p.backend,
        })
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The acting machine's id: the host's own for a local request.
    pub fn machine(&self) -> &str {
        &self.machine
    }

    /// The verified person key id, when a person signature came with the
    /// request (S41); `actor` is then that key's `human:*` person, which
    /// [`Ctx::local`] and [`Ctx::remote`] check.
    pub fn person(&self) -> Option<&str> {
        self.person.as_deref()
    }
}

impl fmt::Debug for Ctx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The notices are the operation's output, not its context.
        f.debug_struct("Ctx")
            .field("origin", &self.origin)
            .field("machine", &self.machine)
            .field("person", &self.person)
            .field("actor", &self.actor)
            .field("agent", &self.agent)
            .field("turn", &self.turn)
            .field("transport", &self.transport)
            .field("idempotency_key", &self.idempotency_key)
            .field("clock", &self.clock)
            .field("trust", &self.trust)
            .field("backend", &self.backend.kind())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::OpSpec;
    use crate::wire::{BackendKind, EventStream, Report, Request};

    struct Nowhere;

    impl Backend for Nowhere {
        fn kind(&self) -> BackendKind {
            BackendKind::Local
        }

        fn apply(&self, _: &Ctx, _: &'static OpSpec, _: &Request) -> Report {
            Report::ok(serde_json::Value::Null)
        }

        fn stream(
            &self,
            _: &Ctx,
            _: &'static OpSpec,
            _: &Request,
        ) -> Result<Box<dyn EventStream>, Report> {
            Err(Report::ok(serde_json::Value::Null))
        }
    }

    fn parts(actor: &str) -> Parts {
        Parts {
            actor: actor.into(),
            agent: None,
            turn: None,
            transport: Transport::Http,
            idempotency_key: None,
            clock: Clock {
                now_ms: 0,
                lease_ttl_ms: 0,
            },
            trust: WorktreeTrust::default(),
            backend: Arc::new(Nowhere),
        }
    }

    #[test]
    fn a_remote_request_cannot_claim_local() {
        // Whatever the request names (a person actor, the host as its
        // transport's peer), its origin is the machine that signed it.
        let verified =
            Verified::for_tests("m-worker", "n1", Some(("pk-1".into(), "human:nick".into())));
        let mut p = parts("human:nick");
        p.transport = Transport::Cli;
        let ctx = Ctx::remote(&verified, p).unwrap();
        assert_eq!(
            ctx.origin(),
            &Origin::Remote {
                machine: "m-worker".into()
            }
        );
        assert_eq!(ctx.machine(), "m-worker");
        assert_eq!(ctx.person(), Some("pk-1"));

        let unsigned = Verified::for_tests("m-worker", "n2", None);
        let ctx = Ctx::remote(&unsigned, parts("human:nick")).unwrap();
        assert_ne!(ctx.origin(), &Origin::Local);
        assert_eq!(ctx.person(), None);

        let host = Ctx::local("m-host".into(), None, parts("human:nick")).unwrap();
        assert_eq!(host.origin(), &Origin::Local);
        assert_eq!(host.machine(), "m-host");
    }

    #[test]
    fn a_person_signature_acts_only_as_its_person() {
        // Ana's key on a request naming human:nick is refused, over the network
        // and on the host alike (SC68).
        let ana = Some(("pk-ana".to_string(), "human:ana".to_string()));
        let verified = Verified::for_tests("m-worker", "n1", ana.clone());
        for actor in ["human:nick", "Human:ana", "serve-skeleton"] {
            let refused = Ctx::remote(&verified, parts(actor)).unwrap_err();
            assert_eq!(refused.code, Code::ApiActorNotAllowed, "{actor}");
            let refused = Ctx::local("m-host".into(), ana.clone(), parts(actor)).unwrap_err();
            assert_eq!(refused.code, Code::ApiActorNotAllowed, "{actor}");
        }
        assert_eq!(
            Ctx::remote(&verified, parts("human:ana")).unwrap().person(),
            Some("pk-ana")
        );
        assert_eq!(
            Ctx::local("m-host".into(), ana, parts("human:ana"))
                .unwrap()
                .person(),
            Some("pk-ana")
        );
    }

    #[test]
    fn reserved_prefixes_match_in_any_case() {
        for actor in ["mdkg:serve", "MDKG:receipts", "launcher:m1", "Launcher:x"] {
            assert!(is_reserved(actor), "{actor}");
        }
        for actor in [
            "mdkg",
            "launcher",
            "a-launcher:x",
            "human:nick",
            "serve-skeleton",
            "é",
        ] {
            assert!(!is_reserved(actor), "{actor}");
        }
    }

    /// Every crate's Rust files, with the ones allowed to build a local
    /// context left out.
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "target") {
                    rust_files(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn local_is_never_built_off_the_host() {
        // S4: no socket request is Local. Only the host's own adapters (the
        // CLI and MCP server in mdkg-cli, the launcher in mdkg-run) may
        // build one; serve's own code, in its crates or in the CLI's
        // serve_cli, may not. A tripwire for the plain path, not a proof:
        // a crate that needs `Ctx::local` joins this list in review.
        const HOST_ADAPTERS: &[&str] = &["mdkg-ops", "mdkg-cli", "mdkg-run"];
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&crates).unwrap() {
            let dir = entry.unwrap().path();
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            if dir.is_dir() && !HOST_ADAPTERS.contains(&name.as_str()) {
                rust_files(&dir, &mut files);
            }
        }
        rust_files(&crates.join("mdkg-cli/src/serve_cli"), &mut files);
        assert!(files.iter().any(|f| f.ends_with("serve_cli/mod.rs")));
        let callers: Vec<_> = files
            .iter()
            .filter(|f| {
                std::fs::read_to_string(f)
                    .unwrap_or_default()
                    .contains("Ctx::local")
            })
            .collect();
        assert!(callers.is_empty(), "Ctx::local off the host: {callers:?}");
    }
}
