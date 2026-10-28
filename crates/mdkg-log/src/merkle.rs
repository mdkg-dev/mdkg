//! Merkle roots over sorted (key, hash) leaves, with inclusion proofs.
//!
//! Every shard has a root from day one. The tree layout can change:
//! roots can be recomputed from content as long as the hash rules hold.
//!
//! - leaf  = sha256(0x00 || key || 0x00 || value_hash)
//! - node  = sha256(0x01 || left || right); an odd node is promoted unchanged
//! - empty = sha256("") (the hash of no bytes)

use mdkg_format::Digest;

fn hash_bytes(bytes: &[u8]) -> Digest {
    Digest::of_bytes(bytes)
}

fn leaf_hash(key: &str, value: &Digest) -> Digest {
    let mut bytes = vec![0u8];
    bytes.extend_from_slice(key.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(value.to_string().as_bytes());
    hash_bytes(&bytes)
}

fn node_hash(left: &Digest, right: &Digest) -> Digest {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(left.to_string().as_bytes());
    bytes.extend_from_slice(right.to_string().as_bytes());
    hash_bytes(&bytes)
}

/// A built tree: leaves sorted by key, all levels kept for proofs.
#[derive(Debug, Clone)]
pub struct MerkleTree {
    keys: Vec<String>,
    levels: Vec<Vec<Digest>>,
}

/// Which side a sibling hash sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Evidence that a (key, value) pair is in a tree with a given root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub key: String,
    pub value: Digest,
    /// Siblings from the leaf up; `None` where a node was promoted.
    pub path: Vec<Option<(Side, Digest)>>,
}

impl MerkleTree {
    /// Builds a tree. Duplicate keys keep the last value.
    pub fn build(mut entries: Vec<(String, Digest)>) -> Self {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|later, earlier| {
            if later.0 == earlier.0 {
                earlier.1 = later.1.clone();
                true
            } else {
                false
            }
        });
        let keys: Vec<String> = entries.iter().map(|(k, _)| k.clone()).collect();
        let mut levels = vec![
            entries
                .iter()
                .map(|(k, v)| leaf_hash(k, v))
                .collect::<Vec<_>>(),
        ];
        while levels.last().is_some_and(|l| l.len() > 1) {
            let below = levels.last().map(Vec::as_slice).unwrap_or_default();
            let next = below
                .chunks(2)
                .map(|pair| match pair {
                    [l, r] => node_hash(l, r),
                    [only] => only.clone(),
                    _ => unreachable!("chunks(2) yields one or two items"),
                })
                .collect();
            levels.push(next);
        }
        Self { keys, levels }
    }

    pub fn root(&self) -> Digest {
        self.levels
            .last()
            .and_then(|l| l.first())
            .cloned()
            .unwrap_or_else(|| hash_bytes(b""))
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// An inclusion proof for `key`, if present.
    pub fn prove(&self, key: &str, value: &Digest) -> Option<Proof> {
        let mut index = self.keys.binary_search_by(|k| k.as_str().cmp(key)).ok()?;
        if self.levels[0][index] != leaf_hash(key, value) {
            return None;
        }
        let mut path = Vec::new();
        for level in &self.levels[..self.levels.len() - 1] {
            let sibling = index ^ 1;
            path.push(level.get(sibling).map(|h| {
                let side = if sibling < index {
                    Side::Left
                } else {
                    Side::Right
                };
                (side, h.clone())
            }));
            index /= 2;
        }
        Some(Proof {
            key: key.to_string(),
            value: value.clone(),
            path,
        })
    }
}

impl Proof {
    /// Whether this proof shows inclusion under `root`.
    pub fn verify(&self, root: &Digest) -> bool {
        let mut hash = leaf_hash(&self.key, &self.value);
        for (side, sibling) in self.path.iter().flatten() {
            hash = match side {
                Side::Left => node_hash(sibling, &hash),
                Side::Right => node_hash(&hash, sibling),
            };
        }
        &hash == root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn d(s: &str) -> Digest {
        Digest::of_bytes(s.as_bytes())
    }

    #[test]
    fn order_of_input_does_not_matter() {
        let a = MerkleTree::build(vec![("b".into(), d("2")), ("a".into(), d("1"))]);
        let b = MerkleTree::build(vec![("a".into(), d("1")), ("b".into(), d("2"))]);
        assert_eq!(a.root(), b.root());
    }

    #[test]
    fn empty_root_is_hash_of_nothing() {
        assert_eq!(MerkleTree::build(vec![]).root(), Digest::of_bytes(b""));
    }

    #[test]
    fn a_changed_value_changes_the_root() {
        let a = MerkleTree::build(vec![("a".into(), d("1")), ("b".into(), d("2"))]);
        let b = MerkleTree::build(vec![("a".into(), d("1")), ("b".into(), d("3"))]);
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn root_vector() {
        // Fixed vector, computed independently: changing the layout or the
        // hash rules changes this value.
        let t = MerkleTree::build(vec![
            ("c".into(), d("3")),
            ("a".into(), d("1")),
            ("b".into(), d("2")),
        ]);
        assert_eq!(
            t.root().to_string(),
            "sha256:3b3bf4d79e083095d1c6e2b9b35ff29dc26358fabba7820e01318315e9800eef"
        );
    }

    proptest! {
        #[test]
        fn every_leaf_proves_and_tampering_fails(n in 1usize..40, pick in 0usize..40) {
            let entries: Vec<(String, Digest)> = (0..n).map(|i| (format!("node-{i:03}"), d(&i.to_string()))).collect();
            let tree = MerkleTree::build(entries.clone());
            let root = tree.root();
            let (key, value) = &entries[pick % n];
            let proof = tree.prove(key, value).expect("present key proves");
            prop_assert!(proof.verify(&root));
            let mut forged = proof.clone();
            forged.value = d("forged");
            prop_assert!(!forged.verify(&root));
            prop_assert!(tree.prove(key, &d("other")).is_none());
        }
    }
}
