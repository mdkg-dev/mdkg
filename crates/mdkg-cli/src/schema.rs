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
use crate::msg::{AckData, DecisionsData, InboxData, RoomData, RoomListData, SendData, ThreadData};
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
    "scratch.path",
    "work.localize",
    "config.show",
    "pack",
    "show",
    "validate",
    "claim",
    "release",
    "renew",
    "holds",
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
    "msg.wait",
    "msg.ack",
    "msg.thread",
    "msg.import",
    "msg.decisions",
    "room.create",
    "room.join",
    "room.leave",
    "room.list",
    "agent",
    "agents.add",
    "agents.edit",
    "agents.rm",
    "agents.up",
    "agents.status",
    "agents.resume",
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
];

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
        "scratch.path" => envelope::<crate::scratch::PathData>(command),
        "work.localize" => envelope::<crate::work::LocalizeData>(command),
        "config.show" => envelope::<crate::config_cmd::ShowData>(command),
        "pack" => envelope::<crate::pack::PackData>(command),
        "show" => envelope::<ShowData>(command),
        "validate" => envelope::<ValidateData>(command),
        "claim" => envelope::<ClaimData>(command),
        "release" => envelope::<ReleaseOutput>(command),
        "renew" => envelope::<RenewData>(command),
        "holds" => envelope::<HoldsData>(command),
        "msg.send" | "msg.import" => envelope::<SendData>(command),
        "msg.decisions" => envelope::<DecisionsData>(command),
        "msg.inbox" | "msg.wait" => envelope::<InboxData>(command),
        "msg.ack" => envelope::<AckData>(command),
        "msg.thread" => envelope::<ThreadData>(command),
        "room.create" | "room.join" | "room.leave" => envelope::<RoomData>(command),
        "room.list" => envelope::<RoomListData>(command),
        "agents.up" => envelope::<crate::agents::UpEvent>(command),
        "agents.status" => envelope::<crate::agents::StatusData>(command),
        "agents.resume" => envelope::<crate::agents::ResumeData>(command),
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
