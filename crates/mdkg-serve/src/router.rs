//! Which handler a request goes to, and which requests no listener ever
//! serves (S4, S19, R42).
//!
//! A route is decided from the head alone, before any body byte is read:
//! the public `GET /`, the machine enrolment routes (S6), the person
//! bootstrap routes (S41), or one operation of the registry by its HTTP
//! template. Anything else is `api.unsupported`. No two routes match one
//! path, and each `{name}` value is decoded once, after the route is
//! chosen (S4's routing and path parameters).
//!
//! Two rules hold on every listener, the Unix socket and TCP alike:
//! - a `host`-class operation is never served: no serve listener produces
//!   `Origin::Local`, and host-class operations run only in-process,
//!   through the direct host CLI or MCP (orchestrator #8984);
//! - `person approve` is served only as the remote approval path, a
//!   request that carries an eligible person's `Present` signature; one
//!   signed by a machine key alone, the host's included, is refused
//!   (#8901, Terra 3 #9112).

use mdkg_format::Code;
use mdkg_graph::ErrorKind;
use mdkg_ops::registry::{Class, Method, OpSpec};

use crate::auth::Authenticated;
use crate::enroll;
use crate::http::RequestHead;
use crate::refusal::Refusal;

/// The operation `person approve` is registered as.
pub const PERSON_APPROVE: &str = "person.approve";

/// Where a request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    /// `GET /`: public fields, unsigned (S4).
    Root,
    /// A machine enrolment route, signed by its pending key (S6).
    Enroll(enroll::Route),
    /// `POST /persons:enroll`: the person bootstrap's first request (S41).
    PersonEnroll,
    /// `POST /persons/{pending_id}:prove`.
    PersonProve(String),
    /// One registry operation, with the template's `{name}` values in
    /// order.
    Op {
        op: &'static OpSpec,
        params: Vec<String>,
    },
}

/// The routes of one `serve`: the fixed ones and `ops`' served entries.
#[derive(Debug, Clone, Copy)]
pub struct Router {
    ops: &'static [OpSpec],
}

fn unsupported(reason: &'static str) -> Refusal {
    Refusal::new(Code::ApiUnsupported, ErrorKind::Usage, reason)
}

impl Router {
    /// Over `ops`, normally `mdkg_ops::registry::OPS`. Two routes that
    /// can match one path are a bug: a panic in a debug build, and
    /// `served_templates_never_overlap` fails on the real registry
    /// (orchestrator #9206).
    pub fn new(ops: &'static [OpSpec]) -> Self {
        debug_assert!(
            overlaps(ops).is_empty(),
            "routes that match one path: {:?}",
            overlaps(ops)
        );
        Self { ops }
    }

    /// The route of `head`, from its method and path alone. A `host`-class
    /// operation is refused here, before anything else is read, whoever
    /// signed it and on whichever listener (#8984).
    pub fn route(&self, head: &RequestHead) -> Result<Routed, Refusal> {
        let method = match head.method.as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            _ => return Err(unsupported("method")),
        };
        // The fixed routes and the registry's are matched together, so
        // a registry entry that overlaps a fixed route is refused as any
        // other overlap, never taken by the fixed one first (Terra 4
        // #9403, Terra 1 #9429). No two can match one path
        // (`Router::new`'s check); should two slip past it in a release
        // build, a path both match is refused, failing closed: never the
        // registry's order, and no precedence between them (orchestrator
        // #9206, #9318).
        let mut found: Option<(Target, Vec<&str>)> = None;
        let mut ambiguous = false;
        let fixed = FIXED.iter().map(|(m, t, f)| (*m, *t, Target::Fixed(*f)));
        let served = self
            .ops
            .iter()
            .filter_map(|op| op.http.map(|r| (r.method, r.path, Target::Op(op))));
        for (m, template, target) in fixed.chain(served) {
            if m != method {
                continue;
            }
            if let Some(raw) = matches(template, &head.path) {
                ambiguous |= found.is_some();
                found = Some((target, raw));
            }
        }
        let (target, raw) = match found {
            None => return Err(unsupported("route")),
            Some(_) if ambiguous => return Err(unsupported("ambiguous_route")),
            Some(f) => f,
        };
        if let Target::Op(op) = target
            && op.class == Class::Host
        {
            return Err(unsupported("host_class"));
        }
        let params: Vec<String> = raw.into_iter().map(decode).collect::<Result<_, _>>()?;
        let fixed = match target {
            Target::Op(op) => return Ok(Routed::Op { op, params }),
            Target::Fixed(f) => f,
        };
        // A fixed route's id is decoded as any capture (S4), then held to
        // the id rules: `%30` is `0` (Terra 4 #9403).
        let id = || {
            params
                .first()
                .filter(|id| enroll::valid_id(id))
                .cloned()
                .ok_or_else(|| unsupported("route"))
        };
        Ok(match fixed {
            Fixed::Root => Routed::Root,
            Fixed::EnrollRequest => Routed::Enroll(enroll::Route::Request),
            Fixed::EnrollReveal => Routed::Enroll(enroll::Route::Reveal(id()?)),
            Fixed::EnrollPoll => Routed::Enroll(enroll::Route::Poll(id()?)),
            Fixed::PersonEnroll => Routed::PersonEnroll,
            Fixed::PersonProve => Routed::PersonProve(id()?),
        })
    }

    /// The checks that need the authenticated request, before the
    /// operation runs: `person approve` only with a person signature that
    /// grades `Present`, never at machine grade, never by a machine key
    /// alone (#8901, #8835, Terra 3 #9112). The kernel decides the rest.
    pub fn admit(&self, routed: &Routed, auth: &Authenticated) -> Result<(), Refusal> {
        let Routed::Op { op, .. } = routed else {
            return Ok(());
        };
        if op.class == Class::Host {
            return Err(unsupported("host_class"));
        }
        if op.name == PERSON_APPROVE {
            match auth.person() {
                Some(p) if !p.machine_grade => {}
                Some(_) => {
                    return Err(Refusal::new(
                        Code::ApiPersonSignatureRequired,
                        ErrorKind::Conflict,
                        "present_required",
                    ));
                }
                None => {
                    return Err(Refusal::new(
                        Code::ApiPersonSignatureRequired,
                        ErrorKind::Conflict,
                        "person_signature",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Whether `path` fits `template` (`/v1/agents/{name}:retire`), with the
/// `{name}` values in order, as sent. A `{name}` takes one whole segment,
/// or the part of the last segment before the template's `:verb`; it is
/// never empty and never holds a `/` or a `:`. Segments are compared as
/// sent, before any decoding, so `%2F` can't split one and `%3A` can't
/// start a verb (A2).
fn matches<'p>(template: &str, path: &'p str) -> Option<Vec<&'p str>> {
    let t: Vec<&str> = template.split('/').collect();
    let p: Vec<&str> = path.split('/').collect();
    if t.len() != p.len() {
        return None;
    }
    let mut params = Vec::new();
    for (ts, ps) in t.iter().zip(&p) {
        match ts.find('{') {
            None => {
                if ts != ps {
                    return None;
                }
            }
            Some(open) => {
                let close = ts.find('}')?;
                let (prefix, suffix) = (&ts[..open], &ts[close + 1..]);
                let value = ps.strip_prefix(prefix)?.strip_suffix(suffix)?;
                if value.is_empty() || value.contains(':') {
                    return None;
                }
                params.push(value);
            }
        }
    }
    Some(params)
}

/// One `{name}` value, percent-decoded exactly once (orchestrator #9262,
/// #9318; Terra 1 #9192, Terra 5 #9293): `codex%3Apr19` is `codex:pr19`,
/// and `agent%2Fserve-core` the branch `agent/serve-core`. The raw path
/// stays as sent for the signature's `@path` (RFC 9421), and handlers
/// never decode again, so `%253A` is the literal `%3A`. A malformed escape,
/// a value that isn't UTF-8, or a decoded control character is refused. A
/// decoded `:` or `/` is allowed, since the route was chosen on the raw
/// path: each resource's own identifier rules judge the value (an agent
/// name refuses `/`, a branch allows it).
fn decode(raw: &str) -> Result<String, Refusal> {
    let invalid = |reason| Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, reason);
    let hex = |b: u8| (b as char).to_digit(16);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            ) else {
                return Err(invalid("path_escape"));
            };
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let value = String::from_utf8(out).map_err(|_| invalid("path_utf8"))?;
    if value.chars().any(char::is_control) {
        return Err(invalid("path_control"));
    }
    Ok(value)
}

/// A fixed route: `GET /`, the enrolment routes (S6) and the person
/// bootstrap's (S41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fixed {
    Root,
    EnrollRequest,
    EnrollReveal,
    EnrollPoll,
    PersonEnroll,
    PersonProve,
}

/// What a matched template routes to.
#[derive(Debug, Clone, Copy)]
enum Target {
    Fixed(Fixed),
    Op(&'static OpSpec),
}

/// The fixed routes, as templates: matched with the registry's, and
/// checked against them for overlap.
const FIXED: &[(Method, &str, Fixed)] = &[
    (Method::Get, "/", Fixed::Root),
    (Method::Post, "/enroll", Fixed::EnrollRequest),
    (Method::Post, "/enroll/{id}:reveal", Fixed::EnrollReveal),
    (Method::Get, "/enroll/{id}", Fixed::EnrollPoll),
    (Method::Post, "/persons:enroll", Fixed::PersonEnroll),
    (
        Method::Post,
        "/persons/{pending_id}:prove",
        Fixed::PersonProve,
    ),
];

/// The enrolment route of `head`, as [`Router::route`] decides it: the
/// enrolment templates matched on the raw path, and the id decoded once
/// and held to the id rules. A path no enrolment template matches, or an
/// id that doesn't decode or isn't valid, is none; the router refuses a
/// malformed escape as such. What auth's pending-key checks read, so they
/// agree with the router (Terra 4 #9403). It matches the fixed templates
/// alone and sees no overlap: the router's result decides, so the
/// listener dispatches enrolment only from [`Routed::Enroll`], never from
/// this on the raw head (Claude #9545).
pub(crate) fn enroll_route(head: &RequestHead) -> Option<enroll::Route> {
    let method = match head.method.as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        _ => return None,
    };
    FIXED.iter().find_map(|(m, template, f)| {
        if *m != method {
            return None;
        }
        let raw = matches(template, &head.path)?;
        let id = || {
            raw.first()
                .and_then(|r| decode(r).ok())
                .filter(|id| enroll::valid_id(id))
        };
        match f {
            Fixed::EnrollRequest => Some(enroll::Route::Request),
            Fixed::EnrollReveal => id().map(enroll::Route::Reveal),
            Fixed::EnrollPoll => id().map(enroll::Route::Poll),
            _ => None,
        }
    })
}

/// Pairs of routes with the same method that some path matches both of,
/// among `ops`' served templates and between them and the fixed routes:
/// every pair is a bug (orchestrator #9206). A literal segment that a
/// `{name}` would also match is one: `GET /v1/agents/search` beside
/// `GET /v1/agents/{name}` would shadow an agent named `search`, so a
/// collection operation takes a `:verb` on the collection segment
/// instead, `GET /v1/agents:search`, which no encoded name can spell
/// (orchestrator #9318, Terra 5 #9293).
pub fn overlaps(ops: &[OpSpec]) -> Vec<(&'static str, &'static str)> {
    let served: Vec<(Method, &'static str)> = ops
        .iter()
        .filter_map(|op| op.http.map(|r| (r.method, r.path)))
        .collect();
    let mut out = Vec::new();
    for (i, (ma, a)) in served.iter().enumerate() {
        let fixed = FIXED.iter().map(|(m, t, _)| (*m, *t));
        for (mb, b) in served[i + 1..].iter().copied().chain(fixed) {
            if *ma == mb && same_path(a, b) {
                out.push((*a, b));
            }
        }
    }
    out
}

/// Whether some path matches both templates: segment by segment, two
/// literals must be equal, two `{name}`s must end alike (the same
/// `:verb`), and a `{name}` must match the literal it meets.
fn same_path(a: &str, b: &str) -> bool {
    let (sa, sb): (Vec<_>, Vec<_>) = (a.split('/').collect(), b.split('/').collect());
    if sa.len() != sb.len() {
        return false;
    }
    sa.iter().zip(&sb).all(|(x, y)| {
        let suffix = |s: &str| s.find('}').map(|i| s[i + 1..].to_owned());
        match (x.contains('{'), y.contains('{')) {
            (false, false) => x == y,
            (true, true) => suffix(x) == suffix(y),
            (true, false) => matches(x, y).is_some(),
            (false, true) => matches(y, x).is_some(),
        }
    })
}

#[cfg(test)]
mod tests;
