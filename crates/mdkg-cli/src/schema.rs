//! `mdkg schema`: JSON Schemas generated from the output types.
//! The schemas in `schemas/` are checked against these
//! by a test, so documentation can't drift from the code.

use std::path::Path;

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema};
use serde_json::{Value, json};

use crate::affected::AffectedData;
use crate::backup::{RestoreData, SnapshotsData};
use crate::coord::{
    AbandonData, ClaimData, HoldsData, ReleaseOutput, RenewData, StatusData, SyncData,
};
use crate::msg::{
    AckData, ChannelData, ChannelListData, DecisionsData, DigestData, ForwardData, InboxData,
    RoomData, RoomListData, SendData, ThreadData,
};
use crate::output::{Envelope, ListData, ShowData, ValidateData, schema_id};
use crate::write::{DoneData, FormatData, NewData, NextData, UpdateData};

/// Every JSON contract, including agent definitions. `cli` is the envelope
/// for arguments that name no command, which never carries data.
pub const COMMANDS: &[&str] = &[
    "list",
    "search",
    "index.rebuild",
    "index.status",
    "graph.refs",
    "graph.backrefs",
    "graph.ready",
    "graph.critical-path",
    "graph.remote.approve",
    "graph.sync",
    "graph.conflicts",
    "graph.resolve",
    "scratch.path",
    "node.edit",
    "node.save",
    "work.localize",
    "config.show",
    "config.set",
    "config.unset",
    "pack",
    "show",
    "validate",
    "claim",
    "release",
    "renew",
    "holds",
    "lease.request",
    "status",
    "sync",
    "abandon",
    "new",
    "done",
    "update",
    "next",
    "goal.current",
    "goal.next",
    "goal.activate",
    "goal.pause",
    "goal.done",
    "goal.claim",
    "format",
    "retype",
    "msg.send",
    "msg.inbox",
    "msg.pending",
    "msg.digest",
    "msg.wait",
    "msg.watch",
    "msg.ack",
    "msg.forward",
    "msg.thread",
    "msg.import",
    "msg.decisions",
    "msg.list",
    "channel.create",
    "channel.join",
    "channel.leave",
    "channel.list",
    "room.create",
    "room.join",
    "room.leave",
    "room.list",
    "agent",
    "hook.event",
    "hook.decision",
    "agents.add",
    "agents.edit",
    "agents.rm",
    "agents.up",
    "agents.prepare",
    "agents.stop",
    "agents.logs",
    "agents.turns",
    "agents.status",
    "agents.list",
    "agents.resume",
    "agents.use-key",
    "install",
    "cli",
    "store.snapshots",
    "store.restore",
    "anchor",
    "export",
    "verify",
    "version",
    "init",
    "doctor",
    "skills.list",
    "skills.show",
    "skills.validate",
    "skills.sync",
    "skills.add",
    "migrate",
    "affected",
    "serve",
    "serve.status",
    "req.list",
    "run.fault",
    "entry.list",
    "entry.show",
    "entry.gate",
    "entry.update",
    "entry.add",
    "entry.ask",
    "entry.answer",
    "entry.decide",
    "entry.propose",
    "entry.complete",
    "entry.fix",
    "entry.verify",
    "entry.reopen",
    "entry.reject",
    "entry.dispute",
    "entry.residual",
    "entry.duplicate",
    "entry.outdated",
    "entry.withdraw",
    "entry.close",
    "entry.resolve",
    "entry.move",
    "entry.req",
];

/// A fault's data part (a-flow A26): the `data` of the `kind: "data"` part
/// of a `fault` message from `mdkg:supervisor`. It names no changed path
/// and quotes no mail body or harness output.
#[derive(JsonSchema)]
#[allow(dead_code)]
struct FaultData {
    /// `mdkg.run.fault.v1`.
    schema: String,
    /// The occurrence's id (a UUID), derived from what it reports, so a
    /// resend is the same occurrence; its copies are
    /// `fault.<fault_id>.r<k>`.
    fault_id: String,
    agent: String,
    /// The error code, such as `run.agent_failed`.
    code: String,
    /// The code's own one-line summary, never an error's free text.
    message: String,
    /// The turn that ended in the error, for a turn's fault.
    turn_id: Option<String>,
    supervisor_id: Option<String>,
    /// The nodes the agent holds leases on.
    nodes: Vec<String>,
    /// The agent's worktree, when it could be read.
    worktree: Option<FaultWorktree>,
    /// When the first occurrence of this agent and code that is still open
    /// was sent, RFC 3339 UTC.
    first_at: String,
    /// How many occurrences of this agent and code the open fault stands
    /// for: one more each time a new one supersedes it.
    #[schemars(range(min = 1))]
    count: u64,
    /// What clears it.
    clears: String,
    /// One command to run next.
    suggested: String,
}

/// A fault's worktree: whether it holds uncommitted changes and how many
/// paths, never which.
#[derive(JsonSchema)]
#[allow(dead_code)]
struct FaultWorktree {
    path: String,
    dirty: bool,
    changed: u64,
}

/// A contract's schema as it is, without the command envelope.
fn raw<T: JsonSchema>(title: &str) -> Schema {
    let mut schema = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>();
    schema.insert("title".into(), json!(title));
    schema
}

fn envelope<T: JsonSchema>(command: &str) -> Schema {
    // The serialize contract: a field the output omits when empty isn't
    // `required`.
    let mut schema = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<Envelope<T>>();
    let id = schema_id(command);
    schema.insert("title".into(), json!(id));
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert(
            "schema".into(),
            json!({ "description": "`mdkg.<command>.v<major>`.", "const": id }),
        );
    }
    schema
}

/// The lease-authority contract this build speaks, for a host's caller to
/// check (orchestrator, 2026-10-21): bumped on any incompatible change to
/// the delegated commands' argv, output or host semantics. 1 is 404001a's:
/// a host's `--mode` only makes a lease stricter, `holds` passes only an
/// exclusive lease, and `.git` is never a scope.
pub const LEASE_AUTHORITY_CONTRACT: u64 = 1;

/// The JSON Schema of a command's output; the commands a lease authority
/// runs carry `x-mdkg-lease-authority`.
pub fn for_command(command: &str) -> Option<Schema> {
    let mut schema = output_schema(command)?;
    if matches!(command, "claim" | "release" | "renew" | "holds") {
        schema.insert(
            "x-mdkg-lease-authority".into(),
            json!(LEASE_AUTHORITY_CONTRACT),
        );
    }
    Some(schema)
}

fn output_schema(command: &str) -> Option<Schema> {
    Some(match command {
        "agent" => {
            let mut schema = SchemaSettings::draft2020_12()
                .into_generator()
                .into_root_schema_for::<mdkg_run::orchestration::agent::Definition>();
            schema.insert("title".into(), json!("mdkg.agent.v1"));
            schema
        }
        // The hook router's contract, not a command's output: what an
        // adapter produces and what the rules decide.
        "hook.event" => raw::<crate::hook::event::HookEvent>("mdkg.hook.event.v1"),
        "hook.decision" => raw::<crate::hook::event::Decision>("mdkg.hook.decision.v1"),
        // A fault message's data part, which `mdkg_run::fault::Fault::parts`
        // builds; not a command's output.
        "run.fault" => {
            let mut schema = raw::<FaultData>("mdkg.run.fault.v1");
            if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                props.insert("schema".into(), json!({ "const": "mdkg.run.fault.v1" }));
            }
            schema
        }
        "agents.add" | "agents.edit" | "agents.rm" => {
            envelope::<crate::agents::DefinitionData>(command)
        }
        "cli" => envelope::<()>(command),
        "list" => envelope::<ListData>(command),
        "search" => envelope::<crate::search::SearchData>(command),
        "index.rebuild" | "index.status" => envelope::<crate::derived::IndexData>(command),
        "graph.refs" | "graph.backrefs" => envelope::<crate::graph::EdgesData>(command),
        "graph.ready" => envelope::<crate::graph::ReadyData>(command),
        "graph.critical-path" => envelope::<crate::graph::CriticalPathData>(command),
        "graph.remote.approve" => envelope::<crate::graph::sync::ApproveData>(command),
        "graph.sync" => envelope::<crate::graph::sync::SyncData>(command),
        "graph.conflicts" => envelope::<crate::graph::sync::ConflictsData>(command),
        "graph.resolve" => envelope::<crate::graph::sync::ResolveData>(command),
        "scratch.path" => envelope::<crate::scratch::PathData>(command),
        "node.edit" => envelope::<crate::coord::node_edit::EditData>(command),
        "node.save" => envelope::<crate::coord::node_edit::SaveData>(command),
        "work.localize" => envelope::<crate::work::LocalizeData>(command),
        "config.show" => envelope::<crate::config_cmd::ShowData>(command),
        "config.set" | "config.unset" => envelope::<crate::config_cmd::set::SetData>(command),
        "pack" => envelope::<crate::pack::PackData>(command),
        "show" => envelope::<ShowData>(command),
        "validate" => envelope::<ValidateData>(command),
        "claim" => envelope::<ClaimData>(command),
        "release" => envelope::<ReleaseOutput>(command),
        "renew" => envelope::<RenewData>(command),
        "holds" => envelope::<HoldsData>(command),
        "lease.request" => envelope::<crate::coord::LeaseRequestData>(command),
        "msg.send" | "msg.import" => envelope::<SendData>(command),
        "msg.decisions" => envelope::<DecisionsData>(command),
        "msg.list" => envelope::<crate::msg::ListData>(command),
        "msg.inbox" | "msg.wait" => envelope::<InboxData>(command),
        "msg.pending" => envelope::<crate::msg::PendingData>(command),
        "msg.digest" => envelope::<DigestData>(command),
        "msg.watch" => envelope::<crate::msg::watch::WatchData>(command),
        "msg.ack" => envelope::<AckData>(command),
        "msg.forward" => envelope::<ForwardData>(command),
        "msg.thread" => envelope::<ThreadData>(command),
        "channel.create" | "channel.join" | "channel.leave" => envelope::<ChannelData>(command),
        "channel.list" => envelope::<ChannelListData>(command),
        "room.create" | "room.join" | "room.leave" => envelope::<RoomData>(command),
        "room.list" => envelope::<RoomListData>(command),
        "agents.prepare" => envelope::<crate::agents::PrepareData>(command),
        "agents.up" => envelope::<crate::agents::UpEvent>(command),
        "agents.stop" => envelope::<crate::agents::StopData>(command),
        "agents.logs" => envelope::<crate::agents::LogsData>(command),
        "agents.turns" => envelope::<crate::agents::TurnsData>(command),
        "agents.status" => envelope::<crate::agents::StatusData>(command),
        "agents.list" => envelope::<crate::agents::ListData>(command),
        "agents.resume" => envelope::<crate::agents::ResumeData>(command),
        "agents.use-key" => envelope::<crate::agents::UseKeyData>(command),
        "install" => envelope::<mdkg_run::install::InstallReport>(command),
        "status" => envelope::<StatusData>(command),
        "sync" => envelope::<SyncData>(command),
        "abandon" => envelope::<AbandonData>(command),
        "new" => envelope::<NewData>(command),
        "done" => envelope::<DoneData>(command),
        "update" => envelope::<UpdateData>(command),
        "next" => envelope::<NextData>(command),
        "goal.current" => envelope::<crate::write::GoalCurrentData>(command),
        "goal.next" => envelope::<crate::write::GoalNextData>(command),
        "goal.activate" | "goal.pause" | "goal.done" => {
            envelope::<crate::write::GoalChangeData>(command)
        }
        "goal.claim" => envelope::<crate::write::GoalClaimData>(command),
        "format" => envelope::<FormatData>(command),
        "affected" => envelope::<AffectedData>(command),
        "retype" => envelope::<crate::retype::RetypeData>(command),
        "store.snapshots" => envelope::<SnapshotsData>(command),
        "store.restore" => envelope::<RestoreData>(command),
        "anchor" => envelope::<crate::anchor::AnchorData>(command),
        "export" => envelope::<crate::export::ExportData>(command),
        "verify" => envelope::<crate::verify::VerifyData>(command),
        "version" => envelope::<crate::version::VersionData>(command),
        "init" => envelope::<crate::init::InitData>(command),
        "doctor" => envelope::<crate::doctor::DoctorData>(command),
        "skills.list" => envelope::<crate::skills::SkillsListData>(command),
        "skills.show" => envelope::<crate::skills::SkillShowData>(command),
        "skills.validate" => envelope::<crate::skills::SkillsValidateData>(command),
        "skills.sync" => envelope::<mdkg_run::install::InstallReport>(command),
        "skills.add" => envelope::<crate::skills::SkillAddData>(command),
        "migrate" => envelope::<crate::migrate::MigrateData>(command),
        "serve" => envelope::<crate::serve_cli::ServeData>(command),
        "serve.status" => envelope::<crate::serve_cli::ServeStatusData>(command),
        "req.list" => envelope::<crate::req::ListData>(command),
        "entry.list" => envelope::<crate::entry::ListData>(command),
        "entry.show" => envelope::<crate::entry::ShowData>(command),
        "entry.gate" => envelope::<crate::entry::GateData>(command),
        // `update` and every op subcommand: what each op line changed.
        "entry.update" | "entry.add" | "entry.ask" | "entry.answer" | "entry.decide"
        | "entry.propose" | "entry.complete" | "entry.fix" | "entry.verify" | "entry.reopen"
        | "entry.reject" | "entry.dispute" | "entry.residual" | "entry.duplicate"
        | "entry.outdated" | "entry.withdraw" | "entry.close" | "entry.resolve" | "entry.move"
        | "entry.req" => envelope::<crate::entry::UpdateData>(command),
        _ => return None,
    })
}

pub fn render(schema: &Schema) -> String {
    let mut text = serde_json::to_string_pretty(schema).unwrap_or_default();
    text.push('\n');
    text
}

pub fn run(command: Option<&str>, out: Option<&Path>) -> u8 {
    if let Some(dir) = out {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("cannot create {}: {e}", dir.display());
            return 4;
        }
        for c in COMMANDS {
            let Some(schema) = for_command(c) else {
                continue;
            };
            let path = dir.join(format!("{}.json", schema_id(c)));
            if let Err(e) = std::fs::write(&path, render(&schema)) {
                eprintln!("cannot write {}: {e}", path.display());
                return 4;
            }
        }
        return 0;
    }
    match command {
        None => {
            for c in COMMANDS {
                println!("{}", schema_id(c));
            }
            0
        }
        Some(c) => match for_command(c) {
            Some(schema) => {
                print!("{}", render(&schema));
                0
            }
            None => {
                eprintln!(
                    "error usage.invalid_arguments: no JSON output for {c}; commands: {}",
                    COMMANDS.join(", ")
                );
                1
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use clap::Subcommand;

    /// Every `entry` subcommand has its schema, so a new one can't reach
    /// the generated files unregistered.
    #[test]
    fn every_entry_subcommand_has_a_schema() {
        let entry = crate::entry::EntryCommand::augment_subcommands(clap::Command::new("entry"));
        let names: Vec<String> = entry
            .get_subcommands()
            .map(|c| format!("entry.{}", c.get_name()))
            .collect();
        assert_eq!(names.len(), 23, "{names:?}");
        for name in &names {
            assert!(super::COMMANDS.contains(&name.as_str()), "{name}");
            assert!(super::for_command(name).is_some(), "{name}");
        }
    }
}
