//! The codes serve adds (the serve edd's "New codes", and API v1's A12
//! codes other than the cursor codes), and how a refusal maps to an HTTP
//! status.

use mdkg_format::{Code, Severity};
use mdkg_graph::ErrorKind;

use crate::registry::OpSpec;
use crate::wire::Report;

/// Every code serve, client mode, machines and people add, in registry
/// order.
pub const SERVE: &[Code] = &[
    Code::ApiOutputGone,
    Code::ApiUnknownResource,
    Code::ApiUnsupported,
    Code::ApiUnauthenticated,
    Code::ApiRefusedByCheck,
    Code::ApiServeRunning,
    Code::ApiKeyRequired,
    Code::ApiKeyExpired,
    Code::ApiTooLarge,
    Code::ApiRateLimited,
    Code::ApiOriginRefused,
    Code::ApiDraining,
    Code::ApiCheckFailed,
    Code::ApiMachineRevoked,
    Code::ApiMachinePending,
    Code::ApiEnrollPending,
    Code::ApiEnrollExists,
    Code::ApiEnrollChanged,
    Code::ApiClockSkew,
    Code::ApiReplayed,
    Code::ApiGraphUnknown,
    Code::ApiActorNotAllowed,
    Code::ApiUnreachable,
    Code::ApiRemoteBound,
    Code::ApiRemoteConflict,
    Code::ApiRemoteAttaching,
    Code::ApiCoordinatorMismatch,
    Code::ApiShownMismatch,
    Code::RunNotPlaced,
    Code::RunMachineLost,
    Code::RunMachineRevoked,
    Code::RunTurnEnded,
    Code::RunRecoveryFailed,
    Code::RunPushRefused,
    Code::RunBadAgentName,
    Code::CoordCommitUnknown,
    Code::CoordBranchNotPushable,
    Code::ApiKeySkewed,
    Code::ApiPersonUnknown,
    Code::ApiPersonSignatureRequired,
    Code::RunNothingToDo,
    // The serve edd's two stated exceptions to #6731 (orchestrator #9241,
    // #9401): 422, Validation.
    Code::ApiAttestationFailed,
    Code::ApiMachineGradeRequired,
];

/// The HTTP status of `op`'s response: A12's table from the exit code, with
/// the exceptions A12 and the serve edd give per code, 201 for a create,
/// and 503 for a busy store. Clients read `ok` and codes, not the status.
pub fn http_status<T>(op: Option<&OpSpec>, report: &Report<T>) -> u16 {
    let Some(kind) = report.failure else {
        return if op.is_some_and(OpSpec::creates) {
            201
        } else {
            200
        };
    };
    if report.busy {
        return 503;
    }
    let first_error = report
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)
        .map(|d| d.code);
    let exception = first_error.and_then(|code| match code {
        Code::ApiOutputGone => Some(410),
        Code::ApiUnknownResource | Code::ApiGraphUnknown => Some(404),
        Code::ApiUnsupported => Some(501),
        Code::ApiUnauthenticated | Code::ApiClockSkew => Some(401),
        Code::ApiRefusedByCheck => Some(403),
        Code::ApiCheckFailed | Code::ApiDraining => Some(503),
        Code::ApiKeyRequired => Some(400),
        Code::ApiRateLimited => Some(429),
        Code::ApiTooLarge => Some(413),
        _ => None,
    });
    exception.unwrap_or(match kind {
        ErrorKind::Usage | ErrorKind::Ambiguous => 400,
        ErrorKind::Validation => 422,
        ErrorKind::NotFound => 404,
        ErrorKind::Failure => 500,
        ErrorKind::Conflict => 409,
        ErrorKind::Timeout => 408,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Class, Keyed, Kind, Method, Person, Route};
    use mdkg_format::Diagnostic;

    #[test]
    fn every_serve_code_is_registered_once() {
        let mut seen = std::collections::HashSet::new();
        for code in SERVE {
            assert!(seen.insert(*code), "{} listed twice", code.as_str());
            assert!(Code::ALL.contains(code));
            assert_eq!(Code::parse(code.as_str()), Some(*code));
        }
    }

    fn refused(kind: ErrorKind, code: Code) -> Report {
        Report::refused(kind, Diagnostic::error(code, "refused"))
    }

    #[test]
    fn statuses_follow_a12() {
        let ok = Report::ok(serde_json::Value::Null);
        assert_eq!(http_status(None, &ok), 200);
        let s = |kind, code| http_status(None, &refused(kind, code));
        assert_eq!(s(ErrorKind::Conflict, Code::RunNotPlaced), 409);
        assert_eq!(s(ErrorKind::Usage, Code::ApiUnsupported), 501);
        assert_eq!(s(ErrorKind::Usage, Code::ApiUnauthenticated), 401);
        assert_eq!(s(ErrorKind::Failure, Code::ApiCheckFailed), 503);
        assert_eq!(s(ErrorKind::NotFound, Code::ApiOutputGone), 410);
        assert_eq!(s(ErrorKind::Timeout, Code::MsgWaitTimeout), 408);
        assert_eq!(s(ErrorKind::Validation, Code::FrontmatterBlankLine), 422);
        assert_eq!(s(ErrorKind::Ambiguous, Code::NodeAmbiguous), 400);
        assert_eq!(s(ErrorKind::Failure, Code::CoordStoreFailed), 500);
        assert_eq!(s(ErrorKind::Validation, Code::ApiAttestationFailed), 422);
        assert_eq!(s(ErrorKind::Validation, Code::ApiMachineGradeRequired), 422);
    }

    #[test]
    fn a_busy_store_is_503_and_a_create_201() {
        let busy = Report::<()>::busy(Diagnostic::error(Code::CoordStoreFailed, "busy"));
        assert_eq!(http_status(None, &busy), 503);

        let mut create = OpSpec {
            name: "channel.create",
            kind: Kind::Write,
            capability: "channels",
            cli: &["channel", "create"],
            mcp: &[],
            http: Some(Route {
                method: Method::Post,
                path: "/v1/channels",
            }),
            input_schema: "mdkg.api.channel.create.request.v1",
            output_schema: "mdkg.api.channel.create.v1",
            keyed: Keyed::Accepted,
            class: Class::Any,
            person: Person::None,
            remote_plan: false,
        };
        let ok = Report::ok(serde_json::Value::Null);
        assert_eq!(http_status(Some(&create), &ok), 201);
        create.http = Some(Route {
            method: Method::Post,
            path: "/v1/channels/{name}:join",
        });
        assert_eq!(http_status(Some(&create), &ok), 200);
        create.kind = Kind::Read;
        create.http = Some(Route {
            method: Method::Post,
            path: "/v1/channels:read",
        });
        assert_eq!(http_status(Some(&create), &ok), 200);
    }
}
