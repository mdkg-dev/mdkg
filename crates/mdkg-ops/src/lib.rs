//! The one kernel behind the CLI, MCP and HTTP (the serve edd, S15 and S16).
//!
//! Every API operation is one entry in [`OPS`], run as `plan`, `apply` and
//! `notes` over an explicit [`Ctx`] instead of process-wide state. In local
//! mode all three run in one process; in client mode `apply` is one HTTP
//! request, which the coordinator answers by running the same `apply`.
//!
//! This crate holds the shared types the serve lanes build on: the
//! registry's [`OpSpec`], the context, the [`wire`] traits that `mdkg-wire`
//! and `mdkg-serve` implement, and the [`codes`] serve adds. The registry
//! lists every CLI operation; in 0.7.1 `serve` runs each as its CLI
//! command (dec-01a18dd9). The operations themselves move here later:
//! every served write as a store operation that takes an open transaction
//! (S16).

pub mod codes;
pub mod ctx;
pub mod registry;
pub mod wire;

pub use ctx::{Clock, Ctx, Notices, Origin, Parts, RESERVED_PREFIXES, Transport, is_reserved};
pub use registry::{
    CAPABILITIES, Class, Keyed, Kind, LONG_POLLS, LongPoll, Method, NOT_OPS, NotOp, OPS, OpSpec,
    Person, Route,
};
pub use wire::{Apply, Backend, BackendKind, Report, Request, Transaction};
