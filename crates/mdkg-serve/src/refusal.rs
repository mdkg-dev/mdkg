//! A transport refusal: the code, why, and the response it becomes.
//! Diagnostics name the failed step, never a key, a signature, a nonce or
//! a body (the serve edd's Security section).

use std::borrow::Cow;

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::ErrorKind;
use serde_json::{Map, Value, json};

use crate::http::Response;

/// The API error envelope's schema: a refusal over HTTP, its `data` an
/// object with `reason`. The remote CLI maps it to its own command's
/// envelope with the same code (orchestrator #7910, #8473, #8726).
pub const ERROR_SCHEMA: &str = "mdkg.api.error.v1";

#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub code: Code,
    pub kind: ErrorKind,
    /// `data.reason`: which rule refused, as a stable word. Always a
    /// `&'static str`, so no request's bytes can reach it: a check's
    /// run-time reason becomes its word in [`CHECK_REASONS`]
    /// (`Refusal::new`; Claude #9460, #9545, Terra 4 #9569).
    pub reason: &'static str,
    /// Other `data` members (`boot_id`, `server_time`).
    pub data: Map<String, Value>,
    /// Seconds, for `Retry-After` (429 and 503).
    pub retry_after: Option<u64>,
    /// A status the code's table doesn't give: 503 for a busy store,
    /// whose `coord.store_failed` is otherwise a 500.
    pub status: Option<u16>,
}

impl Refusal {
    /// A fixed word is kept as it is. A reason a check returned at run
    /// time (orchestrator #9401, Terra 6 #9359) is kept only as its own
    /// word in the closed set [`CHECK_REASONS`]; anything else, which a
    /// check might have read from the request, becomes `check_refused`.
    pub fn new(code: Code, kind: ErrorKind, reason: impl Into<Cow<'static, str>>) -> Self {
        let reason = match reason.into() {
            Cow::Borrowed(r) => r,
            Cow::Owned(r) => CHECK_REASONS
                .iter()
                .copied()
                .find(|w| *w == r)
                .unwrap_or("check_refused"),
        };
        Self {
            code,
            kind,
            reason,
            data: Map::new(),
            retry_after: None,
            status: None,
        }
    }

    /// The reason, for tests' comparisons.
    #[cfg(test)]
    pub(crate) fn test_reason(&self) -> &'static str {
        self.reason
    }

    pub fn unauthenticated(reason: &'static str) -> Self {
        Self::new(Code::ApiUnauthenticated, ErrorKind::Usage, reason)
    }

    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.data.insert(key.to_owned(), value);
        self
    }

    pub fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs.max(1));
        self
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn status(&self) -> u16 {
        self.status.unwrap_or_else(|| {
            let report = mdkg_ops::wire::Report::<()>::refused(
                self.kind,
                Diagnostic::error(self.code, self.reason),
            );
            mdkg_ops::codes::http_status(None, &report)
        })
    }

    /// The refusal as the API error envelope, [`ERROR_SCHEMA`], whose
    /// `data` is an object holding `reason` (A12; orchestrator #8726 on
    /// Terra 3 #8715: `mdkg.cli.v1`'s `data` is null), with its
    /// status. The caller adds `Mdkg-Request-Id`, `Mdkg-Boot` and `Date`.
    pub fn response(&self) -> Response {
        let d = Diagnostic::error(self.code, self.code.summary());
        let d = json!({
            "code": d.code.as_str(),
            "severity": d.severity.as_str(),
            "message": d.message,
        });
        let mut data = self.data.clone();
        data.insert("reason".into(), Value::String(self.reason.into()));
        let body = json!({
            "schema": ERROR_SCHEMA,
            "ok": false,
            "data": data,
            "diagnostics": [d],
        });
        let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
        if let Some(s) = self.retry_after {
            headers.push(("Retry-After".to_owned(), s.to_string()));
        }
        Response {
            status: self.status(),
            headers,
            body: serde_json::to_vec(&body).unwrap_or_default(),
        }
    }
}

/// The words a check may return at run time: S41's attestation reasons,
/// Absent's and then Failed's, each a closed set (orchestrator #9489,
/// #9634, #9830).
pub const CHECK_REASONS: &[&str] = &[
    "none",
    "self",
    "unsupported_format",
    "unsupported_alg",
    "unanchored",
    "backup",
    "malformed",
    "rp_mismatch",
    "no_up",
    "no_uv",
    "key_mismatch",
    "bad_signature",
    "chain_failed",
    "profile",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invalid_msg_wait_after_is_the_api_envelope_with_the_local_code() {
        // Orchestrator #8473 on Terra 2 #8457: over HTTP, remote msg wait's
        // invalid `after` is the standard API refusal envelope, with the
        // code the local CLI gives (`usage.invalid_arguments`), which the
        // remote CLI maps to the local command's envelope (#7910, #8185,
        // #8726).
        let resp = Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "after").response();
        assert_eq!(resp.status, 400);
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["schema"], ERROR_SCHEMA);
        assert!(body["data"].is_object());
        assert_eq!(body["ok"], false);
        assert_eq!(body["diagnostics"][0]["code"], "usage.invalid_arguments");
        assert_eq!(body["data"]["reason"], "after");
    }

    #[test]
    fn a_refusal_is_an_envelope_with_its_status() {
        let r = Refusal::new(Code::ApiRateLimited, ErrorKind::Conflict, "writes")
            .retry_after(0)
            .with("machine", Value::String("m".into()));
        let resp = r.response();
        assert_eq!(resp.status, 429);
        assert!(resp.headers.contains(&("Retry-After".into(), "1".into())));
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["ok"], false);
        assert_eq!(body["data"]["reason"], "writes");
        assert_eq!(body["diagnostics"][0]["code"], "api.rate_limited");
        assert_eq!(Refusal::unauthenticated("unsigned").response().status, 401);
    }

    #[test]
    fn the_two_attestation_codes_are_422_api_errors() {
        // Orchestrator #9241, #9401: an Absent outcome without the
        // acknowledgement, and a Failed one, over HTTP.
        for (code, reason, name) in [
            (
                Code::ApiMachineGradeRequired,
                "backup",
                "api.machine_grade_required",
            ),
            (
                Code::ApiAttestationFailed,
                "chain_failed",
                "api.attestation_failed",
            ),
        ] {
            let resp = Refusal::new(code, ErrorKind::Validation, reason.to_owned()).response();
            assert_eq!(resp.status, 422, "{name}");
            let body: Value = serde_json::from_slice(&resp.body).expect("json");
            assert_eq!(body["schema"], ERROR_SCHEMA);
            assert_eq!(body["diagnostics"][0]["code"], name);
            assert_eq!(body["data"]["reason"], reason, "a closed-set word, kept");
        }
    }

    #[test]
    fn a_reason_known_only_at_run_time_is_the_envelope_s() {
        // Orchestrator #9401, Terra 6 #9359: an attestation's Absent or
        // Failed reason comes from the check, not from a fixed word.
        let reason = ["unsupported", "format"].join("_");
        let r = Refusal::new(Code::ApiMachineGradeRequired, ErrorKind::Validation, reason);
        assert_eq!(r.reason, "unsupported_format");
        let resp = r.response();
        assert_eq!(resp.status, 422);
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["data"]["reason"], "unsupported_format");
        // Anything but a word of the closed set is replaced, never
        // echoed: a request's text that looks like a word included
        // (Terra 4 #9569).
        for bad in [
            "CN=evil",
            "a b",
            "x\n",
            "",
            "secret42",
            "enroll_poll",
            "deadbeef",
        ] {
            let r = Refusal::new(
                Code::UsageInvalidArguments,
                ErrorKind::Validation,
                bad.to_owned(),
            );
            assert_eq!(r.reason, "check_refused", "{bad:?}");
        }
        for word in CHECK_REASONS {
            let r = Refusal::new(
                Code::UsageInvalidArguments,
                ErrorKind::Validation,
                (*word).to_owned(),
            );
            assert_eq!(r.reason, *word);
        }
    }

    /// Every `(code, kind)` pair this crate's own code refuses with: each
    /// `Code::X` followed, after a comma, by `ErrorKind::Y`, and each
    /// `Refusal::unauthenticated`, in the top-level `src/*.rs` files. Test
    /// modules aren't read. Its limit: a refusal built from variables (a
    /// code mapped at run time, say from serve-store's typed
    /// `Refusal::code()`) isn't seen, so such a mapping must pin its own
    /// statuses (Terra 2 #10791). None exists today.
    fn refusals_in_source() -> Vec<(String, String, String)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("src") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let file = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(&path).expect("read");
            let text = text.split("#[cfg(test)]\nmod tests").next().unwrap_or("");
            let ident = |s: &str| -> String {
                s.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect()
            };
            for (i, _) in text.match_indices("Code::") {
                let rest = &text[i + "Code::".len()..];
                let code = ident(rest);
                let after = rest[code.len()..].trim_start();
                let Some(after) = after.strip_prefix(',') else {
                    continue;
                };
                let after = after.trim_start();
                let after = after.strip_prefix("mdkg_graph::").unwrap_or(after);
                if let Some(kind) = after.strip_prefix("ErrorKind::") {
                    out.push((code, ident(kind), file.clone()));
                }
            }
            for _ in text.matches("Refusal::unauthenticated(") {
                out.push(("ApiUnauthenticated".into(), "Usage".into(), file.clone()));
            }
        }
        out
    }

    #[test]
    fn every_code_serve_refuses_with_has_one_status() {
        // #10657 item 2, #10686 item 2: the status comes from
        // mdkg_ops::codes::http_status, from the code where A12 or the
        // serve edd gives one and otherwise from the kind, so a code
        // refused with two kinds could answer with two statuses. Every
        // code this crate refuses with is registered (it is a `Code`) and
        // has exactly one status here, pinned so a change is deliberate.
        // `with_status` overrides (415 for a media type, 503 for a busy
        // store) are per call and not counted.
        let pinned: &[(&str, u16)] = &[
            ("ApiActorNotAllowed", 409),
            ("ApiCheckFailed", 503),
            ("ApiClockSkew", 401),
            ("ApiDraining", 503),
            ("ApiEnrollChanged", 409),
            ("ApiEnrollExists", 409),
            ("ApiEnrollPending", 409),
            ("ApiGraphUnknown", 404),
            ("ApiKeyRequired", 400),
            ("ApiMachinePending", 409),
            ("ApiMachineRevoked", 409),
            ("ApiOriginRefused", 400),
            ("ApiPersonSignatureRequired", 409),
            ("ApiPersonUnknown", 409),
            ("ApiRateLimited", 429),
            ("ApiReplayed", 409),
            ("ApiTooLarge", 413),
            ("ApiUnauthenticated", 401),
            ("ApiUnsupported", 501),
            ("CoordStoreFailed", 500),
            ("UsageInvalidArguments", 400),
        ];
        let code_of = |ident: &str| {
            Code::ALL
                .iter()
                .copied()
                .find(|c| format!("{c:?}") == ident)
                .unwrap_or_else(|| panic!("{ident} isn't a registered code"))
        };
        let kind_of = |ident: &str| match ident {
            "Usage" => ErrorKind::Usage,
            "Ambiguous" => ErrorKind::Ambiguous,
            "Validation" => ErrorKind::Validation,
            "NotFound" => ErrorKind::NotFound,
            "Failure" => ErrorKind::Failure,
            "Conflict" => ErrorKind::Conflict,
            "Timeout" => ErrorKind::Timeout,
            other => panic!("{other} isn't an ErrorKind"),
        };
        let found = refusals_in_source();
        assert!(found.len() > 40, "the scan finds the crate's refusals");
        let mut seen = std::collections::BTreeMap::<String, u16>::new();
        for (code, kind, file) in &found {
            let status = Refusal::new(code_of(code), kind_of(kind), "x").status();
            let want = pinned
                .iter()
                .find(|(c, _)| c == code)
                .map(|(_, s)| *s)
                .unwrap_or_else(|| panic!("{code} ({file}) has no pinned status"));
            assert_eq!(status, want, "{code} with {kind} in {file}");
            seen.insert(code.clone(), status);
        }
        let unused: Vec<_> = pinned
            .iter()
            .filter(|(c, _)| !seen.contains_key(*c))
            .collect();
        assert!(
            unused.is_empty(),
            "pinned but never refused with: {unused:?}"
        );
    }
}
