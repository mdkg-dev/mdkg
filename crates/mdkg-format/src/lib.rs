//! The mdkg file format: node files (markdown with a `---` frontmatter
//! block), identifiers and references, and the canonical forms used for
//! hashing.
//!
//! The format is the kernel's only compatibility promise, so this
//! crate reproduces mdkg 0.6.0 exactly and is checked against the
//! conformance suite (`conformance/`). It does no I/O and builds for WASM,
//! except for minting ids now (the default `mint` feature). Native settings
//! file reads are opt-in through `settings-fs`; the merge/origin engine is pure.
//!
//! Format 0.7 (`docs/format-0.7.md`) adds UUIDv7 identity: [`id`] holds
//! ids, refs and prefixes, and [`handle`] resolves what a user types.

pub mod canonical;
pub mod diag;
pub mod error;
pub mod frontmatter;
pub mod handle;
pub mod hash;
pub mod id;
pub mod ids;
pub mod js;
pub mod node;
pub mod program;
pub mod schema07;
pub mod settings;
pub mod uuid7;

pub use diag::{Code, Diagnostic, Severity};
pub use error::FormatError;
pub use frontmatter::{Frontmatter, Value};
pub use hash::Digest;
pub use id::{IdError, IdPrefix, NodeId, NodeRef, Uuid, Uuid7};
pub use node::{EdgeMap, Node, ParseOptions, TemplateSchema};
