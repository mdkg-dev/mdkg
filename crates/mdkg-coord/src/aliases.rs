//! Readable aliases for nodes, separate from their UUID identity.
//!
//! Identity is the node UUID; `<type>-<number>` aliases are for people. On one
//! machine the local coordinator hands out sequential aliases. Offline, a
//! provisional alias derived from the UUID (`task-3f9a`) is issued, and a
//! sequential one is added on reconnect. Aliases are permanent once issued:
//! every alias ever handed out keeps resolving, so references never need
//! rewriting.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasError {
    /// An alias can name only one node, forever.
    Taken { alias: String, node: String },
}

impl std::fmt::Display for AliasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AliasError::Taken { alias, node } => write!(f, "alias {alias} already names {node}"),
        }
    }
}

impl std::error::Error for AliasError {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aliases {
    next: BTreeMap<String, u64>,
    issued: BTreeMap<String, String>,
}

impl Aliases {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an existing alias (for example when loading a graph). The
    /// next sequential number for its prefix moves past it.
    pub fn record(&mut self, alias: &str, node: &str) -> Result<(), AliasError> {
        if let Some(existing) = self.issued.get(alias)
            && existing != node
        {
            return Err(AliasError::Taken {
                alias: alias.into(),
                node: existing.clone(),
            });
        }
        if let Some((prefix, n)) = alias.rsplit_once('-')
            && let Ok(n) = n.parse::<u64>()
        {
            let next = self.next.entry(prefix.to_string()).or_insert(1);
            *next = (*next).max(n + 1);
        }
        self.issued.insert(alias.to_string(), node.to_string());
        Ok(())
    }

    /// The next sequential alias for `prefix` above `floor` (a number
    /// already in use somewhere the coordinator can't see), issued to `node`.
    pub fn issue_after(&mut self, prefix: &str, node: &str, floor: u64) -> String {
        let next = self.next.entry(prefix.to_string()).or_insert(1);
        *next = (*next).max(floor.saturating_add(1));
        self.issue_sequential(prefix, node)
    }

    /// The next sequential alias for `prefix`, issued to `node`.
    pub fn issue_sequential(&mut self, prefix: &str, node: &str) -> String {
        loop {
            let next = self.next.entry(prefix.to_string()).or_insert(1);
            let alias = format!("{prefix}-{next}");
            *next += 1;
            if !self.issued.contains_key(&alias) {
                self.issued.insert(alias.clone(), node.to_string());
                return alias;
            }
        }
    }

    /// A provisional alias from the node UUID's random tail (`task-p3f9a`),
    /// lengthened until it is unique. It can never equal a sequential alias.
    pub fn issue_provisional(&mut self, prefix: &str, node: &str) -> String {
        let hex: String = node.chars().filter(char::is_ascii_hexdigit).collect();
        // UUIDv7 keeps its random bits at the end, so use the trailing hex.
        for len in 4..=hex.len().max(4) {
            let suffix = &hex[hex.len().saturating_sub(len)..];
            // The `p` marker keeps provisional aliases out of the sequential
            // namespace (`prefix-<digits>`), even when the hex is all digits.
            let alias = format!("{prefix}-p{suffix}");
            match self.issued.get(&alias) {
                Some(existing) if existing == node => return alias,
                Some(_) => continue,
                None => {
                    self.issued.insert(alias.clone(), node.to_string());
                    return alias;
                }
            }
        }
        self.issue_sequential(prefix, node)
    }

    pub fn resolve(&self, alias: &str) -> Option<&str> {
        self.issued.get(alias).map(String::as_str)
    }

    /// Every alias of `node`, in alias order.
    pub fn aliases_of(&self, node: &str) -> Vec<&str> {
        self.issued
            .iter()
            .filter(|(_, n)| *n == node)
            .map(|(a, _)| a.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N1: &str = "0192f4c2-1b2a-7c3d-8e4f-0123456789ab";
    const N2: &str = "0192f4c2-1b2a-7c3d-8e4f-0123456789cd";

    #[test]
    fn sequential_continues_after_recorded_aliases() {
        let mut a = Aliases::new();
        a.record("task-7", N1).unwrap();
        assert_eq!(a.issue_sequential("task", N2), "task-8");
        assert!(matches!(
            a.record("task-7", N2),
            Err(AliasError::Taken { .. })
        ));
    }

    #[test]
    fn provisional_then_sequential_both_keep_resolving() {
        let mut a = Aliases::new();
        let provisional = a.issue_provisional("task", N1);
        assert_eq!(provisional, "task-p89ab");
        let sequential = a.issue_sequential("task", N1);
        assert_eq!(a.resolve(&provisional), Some(N1));
        assert_eq!(a.resolve(&sequential), Some(N1));
        assert_eq!(a.aliases_of(N1).len(), 2);
    }

    #[test]
    fn provisional_aliases_lengthen_on_collision() {
        let mut a = Aliases::new();
        a.record("task-p89ab", N2).unwrap();
        assert_eq!(a.issue_provisional("task", N1), "task-p789ab");
    }
}
