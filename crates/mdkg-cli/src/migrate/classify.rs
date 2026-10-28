//! The plane classifier: which nodes stay in git as
//! knowledge and which move to the coordination plane's log.
//!
//! - Checkpoints are raw work history: they become log records.
//! - A finished test or proof node is evidence: it becomes a check result.
//!   An unfinished one is still planned work, so it stays a work item.
//! - Types whose behavior isn't modeled yet stay as plain nodes, with a warning.
//! - Everything else is knowledge: goals, epics, decisions, designs, rules,
//!   and task, bug, feat and spike definitions.

/// Where a node goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plane {
    /// Stays a node file in git.
    Knowledge,
    /// Leaves git as a `legacy.checkpoint` log record.
    Checkpoint,
    /// Leaves git as a check result.
    CheckResult,
    /// Stays a node file, unchanged in meaning; 0.7.0 doesn't model it yet.
    TierC,
}

/// Node types whose behavior isn't modeled yet: loops, archive sidecars,
/// manifests and the work-contract family.
pub const TIER_C_TYPES: &[&str] = &[
    "loop",
    "archive",
    "manifest",
    "spec",
    "work",
    "work_order",
    "receipt",
    "feedback",
    "dispute",
    "proposal",
];

/// Statuses that mean a test's evidence is final.
const FINISHED: &[&str] = &["done"];

pub fn classify(node_type: &str, status: Option<&str>) -> Plane {
    match node_type {
        "checkpoint" | "chk" => Plane::Checkpoint,
        "test" if status.is_some_and(|s| FINISHED.contains(&s)) => Plane::CheckResult,
        t if TIER_C_TYPES.contains(&t) => Plane::TierC,
        _ => Plane::Knowledge,
    }
}

/// A body section's fate: a progress log or review ledger leaves the node
/// as a review record, with one line left in its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    Progress,
    ReviewLedger,
}

impl SectionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SectionKind::Progress => "progress",
            SectionKind::ReviewLedger => "review-ledger",
        }
    }
}

/// Whether a top-level heading's text starts a section that moves: "Progress"
/// and "Review ledger", in any case and with any suffix such as a date.
/// Evidence sections stay: they are what a reviewer reads to see what proved
/// the work after progress and review history move to the log.
pub fn section_kind(heading: &str) -> Option<SectionKind> {
    let h = heading.trim().to_ascii_lowercase();
    let starts = |word: &str| {
        h.strip_prefix(word)
            .is_some_and(|rest| rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric()))
    };
    if starts("progress") {
        Some(SectionKind::Progress)
    } else if starts("review ledger") {
        Some(SectionKind::ReviewLedger)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planes() {
        assert_eq!(classify("checkpoint", Some("done")), Plane::Checkpoint);
        assert_eq!(classify("test", Some("done")), Plane::CheckResult);
        assert_eq!(classify("test", Some("todo")), Plane::Knowledge);
        assert_eq!(classify("test", None), Plane::Knowledge);
        assert_eq!(classify("loop", Some("todo")), Plane::TierC);
        assert_eq!(classify("work_order", None), Plane::TierC);
        for t in [
            "goal", "epic", "task", "bug", "feat", "spike", "dec", "edd", "rule", "prd",
        ] {
            assert_eq!(classify(t, Some("done")), Plane::Knowledge, "{t}");
        }
    }

    #[test]
    fn sections() {
        assert_eq!(
            section_kind("Progress (2026-10-19)"),
            Some(SectionKind::Progress)
        );
        assert_eq!(section_kind("progress"), Some(SectionKind::Progress));
        assert_eq!(
            section_kind("Progress Evidence"),
            Some(SectionKind::Progress)
        );
        assert_eq!(
            section_kind("Review Ledger"),
            Some(SectionKind::ReviewLedger)
        );
        assert_eq!(
            section_kind("Review ledger (2026-10-21)"),
            Some(SectionKind::ReviewLedger)
        );
        assert_eq!(section_kind("Progressive rollout"), None);
        assert_eq!(section_kind("Review"), None);
        assert_eq!(section_kind("Evidence"), None);
        assert_eq!(section_kind("Completion Evidence"), None);
    }
}
