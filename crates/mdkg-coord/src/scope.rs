//! Lease scopes: a node, a subgraph or a path prefix.
//!
//! Scopes are `/`-separated paths such as `root/<node-id>` or
//! `root/design/`. Two scopes overlap when they are equal or one is a
//! segment-wise prefix of the other, so a lease on a subtree conflicts with a
//! lease on any node inside it.
//!
//! Case never matters: scopes compare, order and hash by
//! their lowercase form, so `README.md` and `readme.md` are one scope on every
//! disk. A scope keeps the spelling it was made with for display and git.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub struct Scope {
    path: String,
    /// The lowercase form every comparison uses.
    key: String,
}

impl PartialEq for Scope {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Scope {}

impl PartialOrd for Scope {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scope {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

impl Hash for Scope {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl From<String> for Scope {
    /// Normalizes, so a stored or received scope is always canonical.
    fn from(path: String) -> Self {
        Scope::new(path)
    }
}

impl From<Scope> for String {
    fn from(scope: Scope) -> Self {
        scope.path
    }
}

impl Scope {
    /// A normalized scope: empty and `.` segments are dropped, `..` folds
    /// into its parent, and leading/trailing slashes are removed, so every
    /// spelling of a path gets the same scope. A `..` above the top is kept as
    /// a literal segment, so it can never alias a real path.
    pub fn new(path: impl Into<String>) -> Self {
        let path: String = path.into();
        let mut segments: Vec<&str> = Vec::new();
        for segment in path.split('/') {
            match segment {
                "" | "." => {}
                ".." if segments.last().is_some_and(|s| *s != "..") => {
                    segments.pop();
                }
                other => segments.push(other),
            }
        }
        let path = segments.join("/");
        // Per character, so context rules (a final sigma) can't make two
        // spellings of one name differ.
        let key = path
            .chars()
            .flat_map(char::to_lowercase)
            .map(|c| if c == 'ς' { 'σ' } else { c })
            .collect();
        Self { path, key }
    }

    /// The spelling it was made with.
    pub fn as_str(&self) -> &str {
        &self.path
    }

    fn segments(&self) -> impl Iterator<Item = &str> {
        self.key.split('/').filter(|s| !s.is_empty())
    }

    /// Whether `self` contains `other` (equal, or a segment-wise prefix).
    pub fn contains(&self, other: &Scope) -> bool {
        let mut theirs = other.segments();
        self.segments().all(|s| theirs.next() == Some(s))
    }

    pub fn overlaps(&self, other: &Scope) -> bool {
        self.contains(other) || other.contains(self)
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_is_segment_wise() {
        let design = Scope::new("root/design/");
        assert!(design.overlaps(&Scope::new("root/design/edd-1")));
        assert!(Scope::new("root/design/edd-1").overlaps(&design));
        assert!(!design.overlaps(&Scope::new("root/designs/edd-1")));
        assert!(!Scope::new("root/task-1").overlaps(&Scope::new("root/task-12")));
        assert!(Scope::new("root/task-1").overlaps(&Scope::new("root/task-1")));
    }

    #[test]
    fn spellings_of_one_path_are_one_scope() {
        let b = Scope::new("root/b");
        for spelling in ["root/./b", "root/a/../b", "/root//b/", "root/b/."] {
            assert_eq!(Scope::new(spelling), b, "{spelling}");
        }
        assert_eq!(Scope::new("../x").as_str(), "../x");
        assert_ne!(Scope::new("../x"), Scope::new("x"));
    }

    #[test]
    fn case_never_matters() {
        assert_eq!(Scope::new("ΟΔΟΣ.md"), Scope::new("οδος.md"), "final sigma");
        assert_eq!(Scope::new("README.md"), Scope::new("readme.md"));
        assert!(Scope::new("SRC").contains(&Scope::new("src/Lib.rs")));
        assert!(Scope::new("docs/\u{212A}.md").overlaps(&Scope::new("DOCS/k.md")));
        assert!(!Scope::new("src").overlaps(&Scope::new("SRC2/x")));
        assert_eq!(
            Scope::new("Src/Lib.rs").as_str(),
            "Src/Lib.rs",
            "spelling kept"
        );
    }
}
