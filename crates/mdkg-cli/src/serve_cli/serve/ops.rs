//! The CLI's operations over HTTP (dec-01a18dd9, step 4): a request is
//! matched to its operation by the registry's route (`mdkg_ops::OPS`,
//! through mdkg-serve's router), its arguments are the path values with
//! the JSON body (a write) or the query (a read), and serve-ops'
//! `SubprocessBackend` runs it as one CLI call. The response is the
//! envelope the CLI printed, with A12's status for its exit code, so the
//! socket answers exactly what the CLI and MCP do.
//!
//! - **Refused before the backend:** a route no operation has, a
//!   host-class operation (the router), a stream other than
//!   `/v1/events` (none is served in 0.7.1), and on loopback a person's
//!   ledger operation (below).
//! - **Refused by the backend:** any class but `any` (operator, host and
//!   decision operations), a decision post, a ledger `decide` or
//!   `residual`, a turn's or a role's lease; and every call once serve's
//!   program was replaced ([`super::binary::Binary::check`]).
//! - **A person's ledger operations on loopback** (orchestrator #12136,
//!   decision 1 (a), part three): the ledger refuses a `human:*` actor's
//!   ops inside a turn, but a CLI child of serve is in no turn, and
//!   loopback can't see the caller's process. So there a `human:*` actor
//!   (any spelling) may make no ledger write (`entry …`, `done
//!   --residual`): those come over the socket, where the caller's process
//!   is checked, or from the CLI.
//! - **A long poll** (`msg wait`) is a request whose call stops when its
//!   client goes away, and as soon as the drain starts (after any turn
//!   grace), answering `api.draining`; any other call runs to its end
//!   unless the drain's 10 s cut stops it (B4).

use std::path::Path;
use std::sync::Arc;

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::ErrorKind;
use mdkg_ops::registry::{Kind, Method, OpSpec};
use mdkg_ops::wire::Backend;
use mdkg_serve::http::HeadExt;
use mdkg_serve::router::{Routed, Router};
use mdkg_wire::codec::Response;
use serde_json::{Map, Value};

use super::binary::Binary;
use super::local::{Peer, Request, envelope, refusal};
use crate::ops_exec::served::{SubprocessBackend, served_ctx};

/// The operations' routes and the backend that runs them.
pub struct Ops {
    router: Router,
    backend: Arc<SubprocessBackend>,
}

impl std::fmt::Debug for Ops {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ops").finish_non_exhaustive()
    }
}

fn usage(message: &str) -> Response {
    refusal(
        ErrorKind::Usage,
        Diagnostic::error(Code::UsageInvalidArguments, message),
    )
}

fn unsupported(message: &str) -> Response {
    refusal(
        ErrorKind::Usage,
        Diagnostic::error(Code::ApiUnsupported, message),
    )
}

/// Whether `actor` is a person's, in any spelling of `human:` (the
/// ledger's rule: a spelling it doesn't admit never slips past).
fn person(actor: &str) -> bool {
    actor
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("human:"))
}

/// A ledger write a person makes: refused on loopback.
fn ledger_write(op: &OpSpec, args: &Value) -> bool {
    (op.name.starts_with("entry.") && op.kind == Kind::Write)
        || (op.name == "node.done" && args.get("residual").is_some_and(|r| !r.is_null()))
}

impl Ops {
    /// For the graph `graph_id`, served from `root`; every call checks
    /// `binary` first.
    pub fn new(root: &Path, graph_id: &str, binary: Arc<Binary>) -> std::io::Result<Self> {
        let backend =
            SubprocessBackend::for_graph(root, graph_id)?.with_check(move || binary.check());
        Ok(Self {
            router: Router::new(mdkg_ops::OPS),
            backend: Arc::new(backend),
        })
    }

    /// The answer to a request no fixed route took.
    pub fn answer(&self, req: &Request<'_>) -> Response {
        let (op, params) = match self.router.route(req.head) {
            Ok(Routed::Op { op, params }) => (op, params),
            Ok(_) => return unsupported("serve-local serves the CLI's operations and GET /"),
            Err(r) => {
                // The router's reason is a fixed word, never the request's.
                let d = Diagnostic::error(
                    r.code,
                    format!("serve doesn't serve this request ({})", r.reason),
                );
                // An unknown route or method is `api.unsupported` (501) by
                // A12 and the serve design, for clients that re-read the
                // capabilities on it; the hint says what's served.
                let d = if r.code == Code::ApiUnsupported {
                    d.with_hint(
                        "serve answers GET /, GET /v1/events and the CLI's operations at \
                         their API v1 paths, by GET or POST; see docs/serve.md",
                    )
                } else {
                    d
                };
                return refusal(r.kind, d);
            }
        };
        if op.kind == Kind::Stream {
            return unsupported("this stream isn't served in 0.7.1; GET /v1/events is");
        }
        let given = match self.arguments(op, req) {
            Ok(given) => given,
            Err(r) => return r,
        };
        let route = op.http.expect("a routed operation has a route");
        let args = match route.bind(params, Some(Value::Object(given))) {
            Ok(args) => args,
            Err(message) => return usage(&message),
        };
        let header = |name: &str| {
            req.head
                .headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        let actor = header("mdkg-actor").unwrap_or_default();
        if req.peer == Peer::Loopback && person(&actor) && ledger_write(op, &args) {
            return super::local::refusal_at(
                403,
                Diagnostic::error(
                    Code::ApiActorNotAllowed,
                    "a person's ledger operations aren't served on loopback",
                )
                .with_hint("make them over the local socket or with the mdkg CLI"),
            );
        }
        let backend: Arc<dyn Backend> = self.backend.clone();
        let ctx = match served_ctx(backend, actor, header("idempotency-key")) {
            Ok(ctx) => ctx,
            Err(d) => return refusal(ErrorKind::Usage, *d),
        };
        let request = mdkg_ops::wire::Request {
            op: op.name,
            body: args,
        };
        // A long poll stops when its client goes or serve stops; any other
        // call only at the drain's cut (B4).
        let cancel = if op.long_poll().is_some() {
            req.cancel
                .long_poll
                .store(true, std::sync::atomic::Ordering::SeqCst);
            &req.cancel.poll
        } else {
            &req.cancel.call
        };
        let report = self.backend.apply_until(&ctx, op, &request, cancel);
        let mut status = mdkg_ops::codes::http_status(Some(op), &report);
        // A claim of a lease you already hold renews it: it creates
        // nothing, so it's 200, not a create's 201 (A12).
        if status == 201
            && report
                .data
                .as_ref()
                .is_some_and(|d| d["data"]["renewed"] == Value::Bool(true))
        {
            status = 200;
        }
        match report.data {
            Some(data) => Response {
                status,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(&data).unwrap_or_default(),
            },
            None => envelope(
                &crate::output::schema_id(&op.cli.join(".")),
                status,
                None,
                &report.diagnostics,
            ),
        }
    }

    /// A read's arguments from its query, a write's from its JSON body;
    /// a read with a body, or a write with a query, is refused.
    fn arguments(&self, op: &OpSpec, req: &Request<'_>) -> Result<Map<String, Value>, Response> {
        let route = op.http.expect("a routed operation has a route");
        match route.method {
            Method::Get => {
                if !req.body.is_empty() {
                    return Err(usage("a read takes its arguments in the query, not a body"));
                }
                let pairs = req
                    .head
                    .query_pairs()
                    .map_err(|_| usage("the query isn't well formed"))?;
                let mut given = Map::new();
                for (name, value) in pairs {
                    match given.get_mut(&name) {
                        None => {
                            given.insert(name, Value::String(value));
                        }
                        Some(Value::Array(all)) => all.push(Value::String(value)),
                        Some(first) => {
                            let first = first.take();
                            given.insert(name, Value::Array(vec![first, Value::String(value)]));
                        }
                    }
                }
                Ok(given)
            }
            Method::Post => {
                if req.head.query.as_deref().is_some_and(|q| !q.is_empty()) {
                    return Err(usage(
                        "a write takes its arguments in the body, not the query",
                    ));
                }
                if req.body.is_empty() {
                    return Ok(Map::new());
                }
                match serde_json::from_slice(req.body) {
                    Ok(Value::Object(m)) => Ok(m),
                    _ => Err(usage("the request body isn't a JSON object")),
                }
            }
        }
    }
}
