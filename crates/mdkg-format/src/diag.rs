//! Diagnostics with stable codes for the versioned output contract.
//!
//! A code names one error condition forever. Codes are added in minor
//! releases and never reused; removing one is a major change. Messages and
//! hints are for people and may change at any time, so callers branch on
//! codes, never on text. Messages quote key names and enum values, never
//! free-text values or body content.

use std::fmt;

macro_rules! codes {
    ($($variant:ident => $name:literal, $summary:literal;)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Code {
            $($variant,)*
        }

        impl Code {
            /// Every code, in registry order.
            pub const ALL: &'static [Code] = &[$(Code::$variant,)*];

            /// The stable dotted name, such as `frontmatter.blank_line`.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Code::$variant => $name,)*
                }
            }

            /// One line on what the code means, for generated docs.
            pub fn summary(self) -> &'static str {
                match self {
                    $(Code::$variant => $summary,)*
                }
            }

            pub fn parse(name: &str) -> Option<Code> {
                Self::ALL.iter().copied().find(|c| c.as_str() == name)
            }
        }
    };
}

codes! {
    UsageInvalidArguments => "usage.invalid_arguments", "The arguments couldn't be parsed.";
    UsageEmptyQuery => "usage.empty_query", "A search query is empty.";
    UsageBadLimit => "usage.bad_limit", "A limit (any --limit or --tail) isn't a positive integer.";
    UsageUnsupportedType => "usage.unsupported_type", "A real node type that this command doesn't handle.";
    UsageUnknownField => "usage.unknown_field", "A field named in --fields doesn't exist.";
    UsageUnknownStatus => "usage.unknown_status", "A status named in --status isn't one any node can have.";
    UsageUnknownType => "usage.unknown_type", "A type named on the command line (--type, new <type>, retype <type>) isn't a node type.";
    UsageUnknownChangeset => "usage.unknown_changeset", "A changeset id isn't in cs-<n> form.";
    UsageBadStatus => "usage.bad_status", "That status can't be set this way.";
    UsageBadValue => "usage.bad_value", "An option value isn't valid for this node type.";
    UsagePathOutsideWorktree => "usage.path_outside_worktree", "A path to claim lies outside the worktree.";

    ConfigMissing => "config.missing", "There is no .mdkg/config.json at the root.";
    ConfigUnreadable => "config.unreadable", "The config file can't be read or isn't JSON.";
    ConfigInvalid => "config.invalid", "The config file breaks a config rule.";
    ConfigUnknownWorkspace => "config.unknown_workspace", "A workspace alias isn't configured.";
    ConfigGraphV2Unsupported => "config.graph_v2_unsupported", "Graph format v2 (.mdkg/graph.json) isn't supported yet.";

    FrontmatterMissingOpen => "frontmatter.missing_open", "The file doesn't start with a --- line.";
    FrontmatterUnclosed => "frontmatter.unclosed", "The closing --- line is missing.";
    FrontmatterBlankLine => "frontmatter.blank_line", "Frontmatter contains a blank line.";
    FrontmatterNotKeyValue => "frontmatter.not_key_value", "A frontmatter line isn't key: value.";
    FrontmatterStrayCr => "frontmatter.stray_cr", "A format 0.7 frontmatter line holds a CR that isn't part of its CRLF ending.";
    FrontmatterInvalidKey => "frontmatter.invalid_key", "A key breaks the key grammar.";
    FrontmatterDuplicateKey => "frontmatter.duplicate_key", "A key appears twice.";
    FrontmatterEmptyValue => "frontmatter.empty_value", "A key has no value.";
    FrontmatterBadList => "frontmatter.bad_list", "A list isn't enclosed in [ ].";
    FrontmatterEmptyListItem => "frontmatter.empty_list_item", "A list has an empty item.";
    FrontmatterTooLarge => "frontmatter.too_large", "Frontmatter exceeds a line, line-length or list-size limit.";

    NodeUnknownType => "node.unknown_type", "The type isn't one the templates define.";
    NodeBadId => "node.bad_id", "The id isn't a lowercase <prefix>-<number> or reserved id.";
    NodeUnknownKey => "node.unknown_key", "A key isn't allowed for this type.";
    NodeWrongKind => "node.wrong_kind", "A value is a list where a string is expected, or the reverse.";
    NodeEmptyField => "node.empty_field", "A required field is empty.";
    NodeNotLowercase => "node.not_lowercase", "A value that must be lowercase isn't.";
    NodeBadDate => "node.bad_date", "A date isn't YYYY-MM-DD.";
    NodeMissingStatus => "node.missing_status", "A work item or decision has no status.";
    NodeBadStatus => "node.bad_status", "The status isn't one the type allows.";
    NodeStatusNotAllowed => "node.status_not_allowed", "This type can't have a status.";
    NodeBadPriority => "node.bad_priority", "The priority isn't an integer in the configured range.";
    NodePriorityNotAllowed => "node.priority_not_allowed", "Only work items have a priority.";
    NodeBadRef => "node.bad_ref", "A reference isn't a valid id, qid or URI.";
    NodeBadSkillSlug => "node.bad_skill_slug", "A skill slug isn't kebab-case.";
    NodeSkillsNotAllowed => "node.skills_not_allowed", "Only work items list skills.";
    NodeBadScope => "node.bad_scope", "A scope entry isn't a valid id.";
    NodeSupersedesNotAllowed => "node.supersedes_not_allowed", "Only decisions can supersede.";
    NodeBadSupersedes => "node.bad_supersedes", "supersedes isn't a dec-<n> id.";
    NodeBadIdentity => "node.bad_identity", "graph_id or node_id isn't a lowercase UUID.";
    NodeIdentityUnsupported => "node.identity_unsupported", "An identity-bearing node needs a v2 graph manifest.";
    NodeBadInteger => "node.bad_integer", "A value isn't a positive integer.";
    NodeDuplicateId => "node.duplicate_id", "Two files in one workspace have the same id.";
    NodeBadLegacyId => "node.bad_legacy_id", "legacy_id isn't a lowercase mdkg 0.6.0 id.";
    NodeDuplicateLegacyId => "node.duplicate_legacy_id", "Two nodes in one workspace have the same legacy_id.";
    NodeExtensionKeyConflict => "node.extension_key_conflict", "An extension key is both a value and the prefix of another key.";
    NodeStaleType => "node.stale_type", "An id or a ref to it names a type the node no longer has; `retype` fixes it.";
    NodeDateOnly => "node.date_only", "created or updated is a date where format 0.7 writes a timestamp.";
    NodeLegacyExtensionKey => "node.legacy_extension_key", "An x_ key, which format 0.7 reads but writes as x-<namespace>.<field>.";
    NodeBadBodyLink => "node.bad_body_link", "A markdown link in the body names a node id that isn't in the graph.";
    NodeNotFound => "node.not_found", "No node has this id or qid.";
    NodeAmbiguous => "node.ambiguous", "A handle (an id, a unique id prefix, a legacy id or a title fragment) names several nodes.";
    NodeResolved => "node.resolved", "A handle that isn't a full id named this node; the message gives its id and title.";
    NodeExists => "node.exists", "A file for the new node already exists.";
    NodeNoneReady => "node.none_ready", "No work item is ready to pick up.";
    NodeNotWork => "node.not_work", "The command needs a work item.";
    NodeHeadingMissing => "node.heading_missing", "The body lacks a heading the type recommends.";
    NodeRawPrompt => "node.raw_prompt", "The body contains a raw prompt marker.";
    NodeRawPayload => "node.raw_payload", "The body contains a raw payload marker.";
    NodeRawSecret => "node.raw_secret", "The body looks like it contains a secret.";
    NodeLegacySpecFile => "node.legacy_spec_file", "A manifest uses the legacy SPEC.md file name.";
    NodeLegacySpecType => "node.legacy_spec_type", "A MANIFEST.md uses the legacy type: spec.";

    GoalBadState => "goal.bad_state", "goal_state isn't a known goal state.";
    GoalConditionTooLong => "goal.condition_too_long", "goal_condition is over 4,000 characters.";
    GoalBadScopeRef => "goal.bad_scope_ref", "A scope_refs entry is empty or isn't an id.";
    GoalEmptyCheck => "goal.empty_check", "A required_checks entry is empty.";
    GoalScopeRefMissing => "goal.scope_ref_missing", "scope_refs names a node that doesn't exist.";
    GoalScopeRefWrongType => "goal.scope_ref_wrong_type", "scope_refs names a node of a type goals can't scope.";
    GoalActiveNodeMissing => "goal.active_node_missing", "active_node names a node that doesn't exist.";
    GoalActiveNodeWrongType => "goal.active_node_wrong_type", "active_node names a node of the wrong type.";
    GoalActiveNodeOutsideScope => "goal.active_node_outside_scope", "active_node isn't inside the goal's scope_refs.";
    GoalMultipleActive => "goal.multiple_active", "More than one root goal is active in a workspace.";
    GoalNoneActive => "goal.none_active", "No goal is active.";
    GoalNotGoal => "goal.not_goal", "The node given where a goal is needed (goal next, activate, pause, done; graph critical-path) isn't a goal.";

    EdgeMissingTarget => "edge.missing_target", "An edge points at a node that doesn't exist.";
    EdgePrevNextAsymmetric => "edge.prev_next_asymmetric", "A prev or next edge has no matching edge back.";
    EdgeCycle => "edge.cycle", "A prev/next chain loops.";

    TemplateMissing => "template.missing", "No template schema exists for the type.";
    TemplateInvalid => "template.invalid", "A template has no type, or its type isn't lowercase.";
    TemplateFallback => "template.fallback", "A bundled template stands in for a missing local one.";

    SkillBothFiles => "skill.both_files", "A skill folder has both SKILL.md and SKILLS.md.";
    SkillMissingFile => "skill.missing_file", "A skill folder has no SKILL.md.";
    SkillLegacyFile => "skill.legacy_file", "A skill uses the legacy SKILLS.md file name.";
    SkillMissingSlug => "skill.missing_slug", "A node lists a skill that doesn't exist.";
    SkillMirrorMissing => "skill.mirror_missing", "A configured skill mirror folder is missing.";
    SkillMirrorManifestMissing => "skill.mirror_manifest_missing", "A skill mirror has no .mdkg-managed.json.";

    IndexLimitExceeded => "index.limit_exceeded", "The graph exceeds an index.limits setting.";

    IoReadFailed => "io.read_failed", "A file or folder can't be read.";
    IoGitFailed => "io.git_failed", "A git command failed.";
    IoWriteFailed => "io.write_failed", "A file can't be written.";

    CoordLeaseHeld => "coord.lease_held", "An overlapping lease is held: someone else's, or one of your own the claim can't renew (a narrower one, a shared one claimed --exclusive, or your turn, which isn't re-entrant).";
    CoordCasFailed => "coord.cas_failed", "The commit lost its compare-and-swap; the lease ended in conflict.";
    CoordPastDeadline => "coord.past_deadline", "The lease's deadline has passed.";
    CoordNotOwner => "coord.not_owner", "The lease named by its id belongs to another actor (a lease over what you act on is coord.lease_held).";
    CoordNotActive => "coord.not_active", "The lease isn't active.";
    CoordNoLease => "coord.no_lease", "The actor holds no active lease on the scope.";
    CoordUnknownLease => "coord.unknown_lease", "No lease has this id.";
    CoordUnknownChangeset => "coord.unknown_changeset", "No changeset has this id.";
    CoordAlreadySettled => "coord.already_settled", "The changeset is already committed or abandoned.";
    CoordOutOfScope => "coord.out_of_scope", "A commit changes a file outside the lease's scope.";
    CoordObservationRaced => "coord.observation_raced", "Main was read before a later change; read it again.";
    CoordKeyReused => "coord.key_reused", "An idempotency key was reused for a different claim.";
    CoordDuplicateLease => "coord.duplicate_lease", "A provisional lease was already reconciled differently.";
    CoordSyncNeeded => "coord.sync_needed", "The worktree lacks unmerged versions in the scope; run sync.";
    CoordSyncFailed => "coord.sync_failed", "Stacking on another branch's version failed.";
    CoordPathMissing => "coord.path_missing", "A claimed path doesn't exist in any worktree yet.";
    CoordLeaseLimit => "coord.lease_limit", "The actor already holds the most leases one actor may; it releases some first.";
    CoordLeaseShared => "coord.lease_shared", "Other actors hold shared leases overlapping yours.";
    CoordNodeDone => "coord.node_done", "The claimed node is already done.";
    CoordContainsAbandoned => "coord.contains_abandoned", "A changeset's commit contains abandoned work, which would reach main with it.";
    CoordStoreFailed => "coord.store_failed", "The coordinator store can't be opened or read.";
    CoordStoreRestored => "coord.store_restored", "The restored store is older than this build: the next command this build runs upgrades it again, copying it first.";
    CoordStoreUpgraded => "coord.store_upgraded", "This command upgraded the coordinator store, after copying it as it was beside it; `store restore` brings the copy back.";
    CoordAuthorityFailed => "coord.authority_failed", "The configured lease authority can't be trusted, run or understood; nothing was granted locally, though the authority may have acted.";
    CoordAuthorityRemote => "coord.authority_remote", "Leases are held by a lease authority; status shows only the local store.";
    CoordAuthorityNote => "coord.authority_note", "A diagnostic from the lease authority with a code this build doesn't know; the message starts with that code.";
    CoordAuthorityIgnored => "coord.authority_ignored", "A lease authority in the committed config is ignored; it's configured per user.";

    // Integrity checks for the store's record chain, anchors and bundles.
    LogBroken => "log.broken", "A store's record chain or a work bundle doesn't verify: a record was changed, dropped or added out of order, or doesn't match its journal entry.";
    LogAnchorMismatch => "log.anchor_mismatch", "The chain no longer holds the record an anchor names: it was rewritten, forked or restored from an older copy after the anchor.";
    LogAnchorMissing => "log.anchor_missing", "There was no anchor to check the chain against; only the chain itself was verified.";
    LogAnchored => "log.anchored", "An anchor of the store's record chain was written (a goal was done); commit it.";
    EdgeTargetNotOnBranch => "edge.target_not_on_branch", "A shared profile C work node names a node this branch doesn't hold, which may be on another branch; or a goal's active_node or last_active_node names a work node this checkout doesn't have, which is in another clone's store.";
    EdgePrivateTarget => "edge.private_target", "A committed node names a profile C work node, which git never holds, so every fresh clone breaks: work refers to design, never the other way round.";
    EdgeBlocksAncestor => "edge.blocks_ancestor", "A node is blocked by a parent, epic or goal above it (written as its blocked_by, or the ancestor's blocks): the two could only wait on each other, so the ready set ignores the edge.";

    MsgHumanActor => "msg.human_actor", "A command a human:* mailbox can't run (a human only acks, reads their own mail and threads, and leaves rooms as themselves), a $MDKG_ACTOR naming anyone but that human, or a --from naming a human.";
    MsgReservedActor => "msg.reserved_actor", "An actor can't be named role:* or run:*; hold a role with a lease instead.";
    MsgBadKind => "msg.bad_kind", "The message kind isn't one v0.1 knows.";
    MsgBadId => "msg.bad_id", "A message id, mailbox, room, role, turn or agent name isn't well formed; a claimed path, or a node in a folder named like a role or a turn, would overlap one; or a role or turn claim is ambiguous with a workspace alias.";
    MsgNoParts => "msg.no_parts", "A message needs at least one part.";
    MsgTooLarge => "msg.too_large", "The message body is over 64 KiB, or an ack's outcome over 500 bytes.";
    MsgSecretRefused => "msg.secret_refused", "The body or an ack's outcome looks like it holds a credential; nothing was stored.";
    MsgIdReused => "msg.id_reused", "The message id is taken by a different message.";
    MsgNoRecipients => "msg.no_recipients", "The message has no recipients besides its sender, its writer and the roles they hold.";
    MsgUnknownMessage => "msg.unknown_message", "No message has this id.";
    MsgUnknownContext => "msg.unknown_context", "No message or context has this id, or a send names a conversation, or replies to a message, the sender isn't part of.";
    MsgNotHolder => "msg.not_holder", "None of your mailboxes received the message.";
    MsgNotShown => "msg.not_shown", "The message hasn't been listed to you yet, so it can't be acked.";
    MsgWaitTimeout => "msg.wait_timeout", "No matching mail arrived before the timeout.";
    RoomUnknown => "room.unknown", "No room has this name.";
    RoomExists => "room.exists", "A room with this name already exists.";
    RoomNotMember => "room.not_member", "The mailbox isn't a member of the room: only members post or add and remove others, and an explicit --member must be a member to leave (leaving yourself again only warns).";
    RoomHumanMember => "room.human_member", "Only the human can remove a human:* member from a room.";

    McpCallTimeout => "mcp.call_timeout", "The tool's CLI call outlasted its timeout and was stopped.";
    McpCliFailed => "mcp.cli_failed", "The tool's CLI call couldn't run or printed no JSON envelope.";

    // mdkg run and install (harness v0.1, M3 and M5)
    RunUnknownAgent => "run.unknown_agent", "No agent has this name yet; start it with --harness and --role.";
    RunAgentFailed => "run.agent_failed", "The agent process failed, timed out, or reported an error.";
    RunSandboxMismatch => "run.sandbox_mismatch", "The agent ran without the sandbox or approval policy its role needs.";
    RunNoReply => "run.no_reply", "The agent finished without a final message to import.";
    RunMailFailed => "run.mail_failed", "A mail command couldn't run, or didn't print the JSON contract.";
    RunIoFailed => "run.io_failed", "The launcher's own files couldn't be read or written.";
    RunUnsupported => "run.unsupported", "v0.1 doesn't run this harness in this role.";
    RunBranchRefused => "run.branch_refused", "A worker's branch is detached or protected, or its turn moved it; or the launcher runs from a checkout of main.";
    RunTooManyFailures => "run.too_many_failures", "--watch stopped after failed turns in a row; the mail waits for a human.";
    RunCwdOutside => "run.cwd_outside", "An agent's folder must be one of the repository's worktrees.";
    InstallUnmergeable => "install.unmergeable", "A config file can't be merged safely, so install left it alone.";
    InstallUnsupportedLayout => "install.unsupported_layout", "install needs the graph at the top of its worktree.";
    InitExists => "init.exists", "The folder already has an mdkg graph (.mdkg/config.json).";
    DoctorCheckFailed => "doctor.check_failed", "A doctor check failed; its record in the output says which and why.";
    DoctorCheckWarned => "doctor.check_warned", "A doctor check found something to look at; its record says what.";
    SkillNotFound => "skill.not_found", "No skill in .mdkg/skills has this slug.";
    SkillInvalid => "skill.invalid", "A SKILL.md's frontmatter lacks name or description, or has a wrong-typed field.";
    SkillMirrorDrift => "skill.mirror_drift", "A mirrored skill is missing or differs from its .mdkg/skills source.";
    SkillMirrorStale => "skill.mirror_stale", "A mirror holds a managed skill whose source is gone.";
    SkillMirrorTargetsUnsupported => "skill.mirror_targets_unsupported", "skills sync writes only .agents/skills and .claude/skills, and the config names other mirror folders.";
    SkillExists => "skill.exists", "The skill is already in .mdkg/skills, changed from what the binary ships; it's kept.";
    InitNeedsMigrate => "init.needs_migrate", "The folder has an mdkg 0.6 graph; migrate moves it to 0.7, and init changes nothing.";
    InitConflict => "init.conflict", "init was given a profile or orchestration name that differs from the graph's own; it changes nothing.";
    InitUnexplained => "init.unexplained", "The folder's .mdkg holds files init didn't write and no config; init leaves it alone.";

    // Migration and compatibility with the 0.6.0 command spellings.
    UsageDeprecated => "usage.deprecated", "A 0.6.0 command spelling ran its 0.7.0 equivalent; the spelling goes away in 0.8.0.";
    MigratePlanMismatch => "migrate.plan_mismatch", "The graph's migration plan isn't the one whose hash was given.";
    MigrateDirty => "migrate.dirty", "The worktree has changes, or the graph changed while the migration ran.";
    MigrateInProgress => "migrate.in_progress", "A migration stopped part way, or another migrate is running; finish it or roll it back.";
    MigrateNotMigrated => "migrate.not_migrated", "There is no migration to roll back or show.";
    MigrateMovedOn => "migrate.moved_on", "HEAD names neither the migration's base nor its commit.";
    MigrateStoreChanged => "migrate.store_changed", "The coordinator store changed since the migration; rolling back would lose that.";
    MigrateUnsupported => "migrate.unsupported", "The graph has a shape or a node this migration can't convert.";
    MigrateAlreadyMigrated => "migrate.already_migrated", "The graph is already on 0.7.0.";
    MigrateStepFailed => "migrate.step_failed", "A migration step failed; the journal says where, and the graph is the old one or the new one.";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// Another place a diagnostic refers to, such as the second file of a
/// duplicate id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    pub line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: Code,
    pub severity: Severity,
    /// Relative to the repository root, with `/` separators.
    pub path: Option<String>,
    /// 1-based.
    pub line: Option<usize>,
    /// The node's qid, when the diagnostic is about one node.
    pub node: Option<String>,
    pub message: String,
    pub hint: Option<String>,
    pub related: Vec<Location>,
    /// Actors the diagnostic is about (a lease's holders), as data: never
    /// in the message, where another actor's chosen name would read as the
    /// tool's own words.
    pub holders: Vec<String>,
}

impl Diagnostic {
    pub fn new(code: Code, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code,
            severity,
            path: None,
            line: None,
            node: None,
            message: message.into(),
            hint: None,
            related: Vec::new(),
            holders: Vec::new(),
        }
    }

    /// Names the actors it's about, as data.
    pub fn held_by(mut self, holders: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.holders.extend(holders.into_iter().map(Into::into));
        self
    }

    pub fn error(code: Code, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Error, message)
    }

    pub fn warning(code: Code, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Warning, message)
    }

    pub fn at(mut self, path: impl Into<String>, line: Option<usize>) -> Self {
        self.path = Some(path.into());
        self.line = line;
        self
    }

    pub fn for_node(mut self, qid: impl Into<String>) -> Self {
        self.node = Some(qid.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn also_at(mut self, path: impl Into<String>, line: Option<usize>) -> Self {
        self.related.push(Location {
            path: path.into(),
            line,
        });
        self
    }

    /// `path:line`, `path`, the node's qid, or nothing.
    pub fn location(&self) -> Option<String> {
        match (&self.path, self.line, &self.node) {
            (Some(p), Some(l), _) => Some(format!("{p}:{l}")),
            (Some(p), None, _) => Some(p.clone()),
            (None, _, Some(n)) => Some(n.clone()),
            (None, _, None) => None,
        }
    }
}

impl fmt::Display for Diagnostic {
    /// `location: severity code: message`, as text mode prints it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(loc) = self.location() {
            write!(f, "{loc}: ")?;
        }
        write!(
            f,
            "{} {}: {}",
            self.severity.as_str(),
            self.code.as_str(),
            self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_unique_dotted_snake_case() {
        let mut seen = HashSet::new();
        for code in Code::ALL {
            let name = code.as_str();
            assert!(seen.insert(name), "duplicate code {name}");
            let (area, rest) = name.split_once('.').expect("area.name");
            for part in [area, rest] {
                assert!(
                    !part.is_empty()
                        && part
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{name} isn't snake_case"
                );
            }
            assert_eq!(Code::parse(name), Some(*code));
            assert!(!code.summary().is_empty());
        }
    }

    #[test]
    fn text_form_names_location_severity_and_code() {
        let d = Diagnostic::error(
            Code::FrontmatterBlankLine,
            "frontmatter lines must not be blank",
        )
        .at(".mdkg/work/task-9.md", Some(5));
        assert_eq!(
            d.to_string(),
            ".mdkg/work/task-9.md:5: error frontmatter.blank_line: frontmatter lines must not be blank"
        );
    }
}
