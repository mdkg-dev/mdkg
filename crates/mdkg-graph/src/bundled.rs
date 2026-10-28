//! Templates bundled with mdkg 0.6.0, used when a graph's own template set
//! lacks a type (the reference implementation does the same).

/// (file name without `.md`, contents)
pub const BUNDLED_DEFAULT: &[(&str, &str)] = &[
    ("archive", include_str!("../bundled/default/archive.md")),
    ("bug", include_str!("../bundled/default/bug.md")),
    ("chk", include_str!("../bundled/default/chk.md")),
    ("dec", include_str!("../bundled/default/dec.md")),
    ("dispute", include_str!("../bundled/default/dispute.md")),
    ("edd", include_str!("../bundled/default/edd.md")),
    ("epic", include_str!("../bundled/default/epic.md")),
    ("feat", include_str!("../bundled/default/feat.md")),
    ("feedback", include_str!("../bundled/default/feedback.md")),
    ("goal", include_str!("../bundled/default/goal.md")),
    ("loop", include_str!("../bundled/default/loop.md")),
    ("manifest", include_str!("../bundled/default/manifest.md")),
    ("prd", include_str!("../bundled/default/prd.md")),
    ("prop", include_str!("../bundled/default/prop.md")),
    ("proposal", include_str!("../bundled/default/proposal.md")),
    ("receipt", include_str!("../bundled/default/receipt.md")),
    ("rule", include_str!("../bundled/default/rule.md")),
    ("spec", include_str!("../bundled/default/spec.md")),
    ("spike", include_str!("../bundled/default/spike.md")),
    ("task", include_str!("../bundled/default/task.md")),
    ("test", include_str!("../bundled/default/test.md")),
    ("work", include_str!("../bundled/default/work.md")),
    (
        "work_order",
        include_str!("../bundled/default/work_order.md"),
    ),
];
