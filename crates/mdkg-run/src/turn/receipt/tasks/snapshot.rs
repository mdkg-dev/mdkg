//! Immutable graph configuration for task receipts. Capture examines one full
//! committed tree (at most 100,000 entries), then reads config blobs in batches.
//! The phase has its own aggregate 30 s / 16 MiB budget and 1 MiB blob cap.
//! Consumers select the active graph roots; unrelated fixture graphs are data.
//! Selected config changes, including changes reverted at HEAD, are unavailable.

use super::*;

#[derive(Debug)]
pub struct Snapshot {
    span: GitSpan,
    configs: BTreeMap<String, Vec<u8>>,
    entries: BTreeMap<String, Version>,
}

fn policy(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path == ".mdkg/config.json"
        || path.ends_with("/.mdkg/config.json")
        || path == ".mdkg/graph.json"
        || path.ends_with("/.mdkg/graph.json")
}

fn root(path: &str) -> &str {
    let suffix = if path.to_ascii_lowercase().ends_with("/graph.json") {
        ".mdkg/graph.json"
    } else {
        ".mdkg/config.json"
    };
    path[..path.len() - suffix.len()].trim_end_matches('/')
}

impl Snapshot {
    pub fn capture(pin: &Pinned, span: &GitSpan) -> Result<Self, RunError> {
        Self::capture_with(pin, span, Limits::default())
    }

    fn capture_with(pin: &Pinned, span: &GitSpan, limits: Limits) -> Result<Self, RunError> {
        let mut git = Reader::new(pin, limits)?;
        let expected = (span.branch.clone(), span.head.clone());
        if git.head()? != expected {
            return Err(unavailable("HEAD changed before configuration capture"));
        }
        let head = span
            .head
            .as_deref()
            .filter(|h| object_id(h))
            .ok_or_else(|| unavailable("no committed graph configuration"))?;
        let raw = git.ok(&["ls-tree", "-r", "--full-tree", "-z", head])?;
        let rows = tree_entries(&raw)?;
        if rows.len() > git.limits.changes {
            return Err(unavailable("configuration tree-entry count limit exceeded"));
        }
        let mut ids = BTreeSet::new();
        let mut entries = BTreeMap::new();
        for (path, version) in rows {
            if policy(&path)
                && path.ends_with("/config.json")
                && matches!(version.mode.as_str(), "100644" | "100755")
            {
                ids.insert(version.object.clone());
            }
            if entries.insert(path, version).is_some() {
                return Err(unavailable("duplicate tree path"));
            }
        }
        let blobs = read_blobs(&mut git, ids)?;
        let mut configs = BTreeMap::new();
        let mut expanded = 0_usize;
        for (path, version) in entries.iter().filter(|(p, v)| {
            policy(p)
                && p.ends_with("/config.json")
                && matches!(v.mode.as_str(), "100644" | "100755")
        }) {
            let blob = &blobs[&version.object];
            expanded = expanded.saturating_add(blob.len());
            if configs.len() >= 1024 || expanded > git.limits.bytes {
                return Err(unavailable("configuration expansion limit exceeded"));
            }
            configs.insert(path.clone(), blob.clone());
        }
        if git.head()? != expected {
            return Err(unavailable("HEAD changed during configuration capture"));
        }
        Ok(Self {
            span: span.clone(),
            configs,
            entries,
        })
    }

    /// Includes removed/changed policy paths so a vanished nested graph cannot
    /// silently change identity interpretation in earlier commits.
    pub fn policy_roots(&self) -> BTreeSet<String> {
        self.entries
            .keys()
            .chain(
                self.span
                    .commits
                    .iter()
                    .flat_map(|c| &c.changes)
                    .map(|c| &c.path),
            )
            .filter(|p| policy(p))
            .map(|p| root(p).to_owned())
            .collect()
    }

    /// Exact tracked leaf spellings, including paths removed during the span.
    /// The graph adapter compares these with its coordinator scope semantics
    /// before using case-sensitive literal Git pathspecs.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str).chain(
            self.span
                .commits
                .iter()
                .flat_map(|c| &c.changes)
                .map(|c| c.path.as_str()),
        )
    }

    pub fn config(&self, graph_root: &str) -> Result<&[u8], String> {
        if self
            .span
            .commits
            .iter()
            .flat_map(|c| &c.changes)
            .any(|c| policy(&c.path) && root(&c.path).eq_ignore_ascii_case(graph_root))
        {
            return Err("graph configuration changed during the span".into());
        }
        let prefix = if graph_root.is_empty() {
            String::new()
        } else {
            format!("{graph_root}/")
        };
        let expected = format!("{prefix}.mdkg/config.json");
        for path in self.entries.keys().filter(|p| policy(p)) {
            if root(path).eq_ignore_ascii_case(graph_root) && path != &expected {
                return Err("legacy manifest or ambiguous graph configuration spelling".into());
            }
        }
        self.configs
            .get(&expected)
            .map(Vec::as_slice)
            .ok_or_else(|| "graph config is absent or is not a regular blob".into())
    }

    pub fn validate(&self, span: &GitSpan) -> Result<(), String> {
        if &self.span == span {
            Ok(())
        } else {
            Err("configuration belongs to another span".into())
        }
    }

    /// Git trees cannot contain a file/link/gitlink and its descendants.
    /// An absent directory is allowed, but an obstructing ancestor is not.
    pub fn directory(&self, path: &str) -> Result<(), String> {
        if path.len() > 4096 || !valid_path(path) {
            return Err("invalid snapshot folder".into());
        }
        let mut parts = path.split('/');
        let mut prefix = String::new();
        for part in &mut parts {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if self.entries.contains_key(&prefix) {
                return Err("snapshot folder is obstructed by a file, link or submodule".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::receipt::{harvest, tests::Repo};
    use std::{fs, time::Duration};

    fn fixture() -> (Repo, GitSpan) {
        let repo = Repo::new("sha1");
        repo.write(".mdkg/config.json", b"{\"fixture\":true}");
        let start = repo.commit();
        repo.write("README", b"change");
        repo.commit();
        let span = harvest(&repo.pin(), Some(&start)).unwrap();
        (repo, span)
    }

    #[test]
    fn captured_config_is_immutable_and_bound_to_its_exact_span() {
        let (repo, span) = fixture();
        let snapshot = Snapshot::capture(&repo.pin(), &span).unwrap();
        repo.write(".mdkg/config.json", b"different checkout bytes");
        assert_eq!(snapshot.config("").unwrap(), b"{\"fixture\":true}");
        assert!(snapshot.validate(&span).is_ok());
        let mut other = span.clone();
        other.start = None;
        assert!(snapshot.validate(&other).is_err());
        repo.commit();
        assert!(Snapshot::capture(&repo.pin(), &span).is_err());
    }

    #[test]
    fn capture_capacity_refusals_are_not_an_empty_snapshot() {
        let (repo, span) = fixture();
        for limits in [
            Limits {
                time: Duration::ZERO,
                ..Limits::default()
            },
            Limits {
                bytes: 1,
                ..Limits::default()
            },
            Limits {
                changes: 1,
                ..Limits::default()
            },
        ] {
            let error = Snapshot::capture_with(&repo.pin(), &span, limits).unwrap_err();
            assert!(error.message.contains("limit"), "{}", error.message);
        }
    }

    #[test]
    fn absent_config_blob_refuses_capture_instead_of_using_the_checkout() {
        let (repo, span) = fixture();
        let id = repo.git(&["rev-parse", "HEAD:.mdkg/config.json"]);
        fs::remove_file(repo.0.join(".git/objects").join(&id[..2]).join(&id[2..])).unwrap();
        assert!(Snapshot::capture(&repo.pin(), &span).is_err());
    }
}
