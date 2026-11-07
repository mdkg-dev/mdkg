//! The harness seam's kernel (edd-01a1a454, "The seam"): one agent turn as
//! the operations a runner calls, in their order.
//!
//! ```text
//! admit -> starting -> spawned -> output* -> import? -> end
//! ```
//!
//! Step 1 is in-process: each operation is a Rust function over the types
//! here, and the in-tree runner ([`crate::turn`], with each harness behind
//! [`crate::harness::HarnessAdapter`]) is their only caller, as an external
//! runner will be a client of their registry rows. There are no rows,
//! schemas, transports or commands yet, and no behaviour changes: each
//! function is today's code, moved.
//!
//! **The kernel keeps** the policy, the mail and its conversation, the
//! prompt, the turn number and the index, the journal's own events, durable
//! progress, shows and set-aside, the import with its acks, the receipt,
//! faults and the run lease. **The runner keeps** the program and its
//! arguments, its settings, its login and billing, its sessions, its native
//! output and the parsing of it, and its capacity and usage-limit handling.
//!
//! Nothing here names a harness: a harness is an opaque name, and a session
//! an opaque token.

/// Whether the harness took the whole prompt (the edd's D-H19). Step 1
/// knows two of the three: [`PromptDelivered::True`] when the runner saw
/// every byte written (or passed the prompt in the argument list), and
/// [`PromptDelivered::Unconfirmed`] otherwise. Only step 2's runners report
/// [`PromptDelivered::False`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PromptDelivered {
    True,
    False,
    #[default]
    Unconfirmed,
}

impl PromptDelivered {
    /// Only positive evidence counts as delivered.
    pub fn delivered(self) -> bool {
        self == Self::True
    }
}

impl From<bool> for PromptDelivered {
    fn from(delivered: bool) -> Self {
        if delivered {
            Self::True
        } else {
            Self::Unconfirmed
        }
    }
}

/// What the runner says happened to a turn (the edd's H4); the kernel
/// derives the outcome from it and the import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnResult {
    /// The harness ran and exited on its own.
    Ended,
    /// The harness ran and failed.
    Failed,
    /// The runner, not the turn, stopped it.
    Interrupted,
    /// Nothing ran.
    NotStarted,
}

mod admit;
mod end;
mod import;
pub mod live;
mod output;

pub(crate) use admit::{AdmitRequest, Admitted, Grant, RecordRequest, Recorded, admit, record};
pub(crate) use end::{Closed, end};
pub(crate) use import::{ImportRequest, import};
pub(crate) use live::{StartingRequest, spawned, starting};
pub(crate) use output::output;

use std::path::PathBuf;

use crate::turn::{journal, receipt};

/// One turn as the kernel holds it between its operations. In-process it
/// lives on the runner's stack for the length of the turn; step 2 keeps it
/// in the store, keyed by the turn id.
#[derive(Default)]
pub(crate) struct Turn<'call, 'saved> {
    pub(crate) watch: Option<&'call mut crate::supervise::TurnWatch<'saved>>,
    pub(crate) claimed: bool,
    pub(crate) recovery_in_prompt: bool,
    pub(crate) prompt_delivered: bool,
    pub(crate) receipt: Option<receipt::delivery::Admission>,
    pub(crate) receipt_status: Option<receipt::delivery::DeliveryStatus>,
    pub(crate) process_exits: Vec<serde_json::Value>,
    pub(crate) cleanup: Option<(PathBuf, PathBuf)>,
    /// The attempt's turn id once its index start line is written (A7).
    pub(crate) indexed: Option<String>,
    /// Its output journal (A11), once created.
    pub(crate) journal: Option<journal::Writer>,
    /// The mail whose shows this turn counted (friction 62).
    pub(crate) counted: Vec<String>,
    /// The turn failed because of what it ran, so its shows stand: the
    /// harness exited non-zero (not at capacity or the usage limit), its
    /// output didn't parse, or the store refused the reply's content.
    pub(crate) own_failure: bool,
}
