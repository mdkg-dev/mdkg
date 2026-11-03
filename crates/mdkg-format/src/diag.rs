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
    ApiBadCursor => "api.bad_cursor", "The event cursor isn't well formed.";
    ApiCursorReset => "api.cursor_reset", "The cursor's store history was restored, replaced, or rolled back.";
    ApiCursorForeign => "api.cursor_foreign", "The cursor names another coordinator store.";
    ApiCursorExpired => "api.cursor_expired", "The records after the cursor were pruned.";
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

    MsgHumanActor => "msg.human_actor", "A command a human:* mailbox can't run (a human only acks, reads their own mail and threads, and leaves channels as themselves), a $MDKG_ACTOR naming anyone but that human, or a --from naming a human.";
    MsgReservedActor => "msg.reserved_actor", "An actor can't be named role:* or run:*; hold a role with a lease instead.";
    MsgBadKind => "msg.bad_kind", "The message kind isn't one this build knows, or is one only mdkg sends (receipt, fault).";
    MsgBadId => "msg.bad_id", "A message id, mailbox, channel, role, turn or agent name isn't well formed; a claimed path, or a node in a folder named like a role or a turn, would overlap one; or a role or turn claim is ambiguous with a workspace alias.";
    MsgNoParts => "msg.no_parts", "A message needs at least one part.";
    MsgTooLarge => "msg.too_large", "The message body is over 64 KiB, or an ack's outcome over 500 bytes.";
    MsgSecretRefused => "msg.secret_refused", "The body or an ack's outcome looks like it holds a credential; nothing was stored.";
    MsgIdReused => "msg.id_reused", "The message id is taken by a different message.";
    MsgNoRecipients => "msg.no_recipients", "The message has no recipients besides its sender, its writer and the roles they hold.";
    MsgUnknownMessage => "msg.unknown_message", "No message has this id.";
    MsgUnknownContext => "msg.unknown_context", "No message or context has this id, or a send names a conversation, or replies to a message, the sender isn't part of.";
    MsgNotHolder => "msg.not_holder", "None of your mailboxes received the message.";
    MsgNotShown => "msg.not_shown", "The message hasn't been listed to you yet, so it can't be acked.";
    MsgNotSender => "msg.not_sender", "A send supersedes a message another sender sent: only its sender replaces a message; nothing was sent.";
    MsgWaitTimeout => "msg.wait_timeout", "No matching mail arrived before the timeout.";
    ChannelUnknown => "channel.unknown", "No channel has this name.";
    ChannelExists => "channel.exists", "A channel with this name already exists.";
    ChannelNotMember => "channel.not_member", "The mailbox isn't a member of the channel.";
    ChannelHumanMember => "channel.human_member", "Only the human can remove a human:* member from a channel.";
    RoomUnknown => "room.unknown", "No room has this name.";
    RoomExists => "room.exists", "A room with this name already exists.";
    RoomNotMember => "room.not_member", "The mailbox isn't a member of the room: only members post or add and remove others, and an explicit --member must be a member to leave (leaving yourself again only warns).";
    RoomHumanMember => "room.human_member", "Only the human can remove a human:* member from a room.";

    // mdkg flow: the ledger and its gate (edd-01a1878e, A27, A29, A32)
    EntryBadOp => "entry.bad_op", "A ledger op object doesn't parse: an unknown op or field, a wrong type, or a missing field.";
    EntryBadText => "entry.bad_text", "An entry's title isn't 1 to 200 bytes on one line, or a text is over 4 KiB.";
    EntryNotFound => "entry.not_found", "No entry has this id on the node.";
    EntryNoNode => "entry.no_node", "An entry is named without a node, and none is implied.";
    EntryBadTransition => "entry.bad_transition", "The entry's state doesn't allow this op; terminal entries take none.";
    EntryNotAllowed => "entry.not_allowed", "The actor's class (worker, reviewer, person, coordinator, other) may not make this op.";
    EntryFixAfterRange => "entry.fix_after_range", "A reopen's reviewed SHA doesn't hold the finding's fix: the fix landed after the range.";
    EntryNotCovered => "entry.not_covered", "A SHA doesn't hold a finding's fix, or the gated head doesn't hold a verified fix or its reviewed chain.";
    EntryRangeMismatch => "entry.range_mismatch", "A review.complete names a range other than its round's.";
    EntryUnknownCommit => "entry.unknown_commit", "A SHA isn't in the repository, or an abbreviation isn't unique.";
    EntryGit => "entry.git", "Git couldn't answer an ancestry or content check.";
    EntryGateFailed => "entry.gate_failed", "done refused: the node's gate fails, and no residual may settle what fails.";
    LedgerOutOfScope => "ledger.out_of_scope", "A ledger block's op names a node without a card in the turn, or an entry the turn wasn't shown.";

    McpCallTimeout => "mcp.call_timeout", "The tool's CLI call outlasted its timeout and was stopped.";
    McpCliFailed => "mcp.cli_failed", "The tool's CLI call couldn't run or printed no JSON envelope.";

    // mdkg run and install (harness v0.1, M3 and M5)
    RunUnknownAgent => "run.unknown_agent", "No agent has this name yet; start it with --harness and --role.";
    RunAgentFailed => "run.agent_failed", "The agent process failed, timed out, or reported an error.";
    RunSandboxMismatch => "run.sandbox_mismatch", "The agent ran without the sandbox or approval policy its role needs.";
    RunNoReply => "run.no_reply", "The agent finished without a final message to import.";
    RunMailFailed => "run.mail_failed", "A mail command couldn't run, or didn't print the JSON contract.";
    RunProgressInvalid => "run.progress_invalid", "Supervisor progress is corrupt, foreign, unsafe or cannot advance.";
    RunTurnInterrupted => "run.turn_interrupted", "An interrupted turn must be inspected and explicitly resumed before execution.";
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
    SkillRefMode => "skill.ref_mode", "skills sync and skills add write a checkout's .mdkg/skills, which a ref-mode checkout lacks; install refreshes the skill mirrors there.";
    SkillExists => "skill.exists", "The skill is already in .mdkg/skills, changed from what the binary ships; it's kept.";
    InitNeedsMigrate => "init.needs_migrate", "The folder has an mdkg 0.6 graph; migrate moves it to 0.7, and init changes nothing.";
    InitConflict => "init.conflict", "init was given a profile or orchestration name that differs from the graph's own; it changes nothing.";
    InitUnexplained => "init.unexplained", "The folder's .mdkg holds files init didn't write and no config; init leaves it alone.";
    InitGraphFetched => "init.graph_fetched", "A fetched ref (any refs/remotes/* with a committed config) holds a graph of this repository, or git couldn't list them; init starts no second one: a branch that predates a graph the team merges (checkout mode) switches to or merges the default branch, and install adopts a graph kept on its own ref (ref mode).";

    // Migration and compatibility with the 0.6.0 command spellings.
    UsageDeprecated => "usage.deprecated", "A 0.6.0 command spelling ran its 0.7.0 equivalent; the spelling goes away in 0.8.0.";
    MigratePlanMismatch => "migrate.plan_mismatch", "The graph's migration plan isn't the one whose hash was given.";
    MigrateDirty => "migrate.dirty", "The worktree has changes, or the graph changed while the migration ran.";
    MigrateInProgress => "migrate.in_progress", "A migration stopped part way, or another migrate is running; finish it or roll it back.";
    MigrateNotMigrated => "migrate.not_migrated", "There is no migration to roll back or show.";
    MigrateMovedOn => "migrate.moved_on", "HEAD names neither the migration's base nor its commit.";
    MigrateStoreChanged => "migrate.store_changed", "The coordinator store changed since the migration; rolling back would lose that.";
    MigrateUnsupported => "migrate.unsupported", "The graph has a shape or a node this migration can't convert.";
    MigrateAlreadyMigrated => "migrate.already_migrated", "The graph is already on format 0.7.";
    MigrateStepFailed => "migrate.step_failed", "A migration step failed; the journal says where, and the graph is the old one or the new one.";

    // mdkg serve, client mode and machines (the serve edd, edd-01a18803).
    // API v1's codes other than its cursor codes, which a-events adds.
    ApiOutputGone => "api.output_gone", "The turn's output was purged; the receipt is the lasting record.";
    ApiUnknownResource => "api.unknown_resource", "No resource has this path.";
    ApiUnsupported => "api.unsupported", "This build, or this coordinator, doesn't implement the operation or capability.";
    ApiUnauthenticated => "api.unauthenticated", "A remote request without a valid signature from an enrolled machine.";
    ApiRefusedByCheck => "api.refused_by_check", "The configured outside check refused the request.";
    ApiServeRunning => "api.serve_running", "A serve process already holds this repository's serve lock.";
    ApiKeyRequired => "api.key_required", "A write over the network needs an idempotency key.";
    ApiKeyExpired => "api.key_expired", "The idempotency key is over 23 hours old, so a retry could run as a new write; the client refuses it.";
    ApiTooLarge => "api.too_large", "A request's body or headers are over serve's limits.";
    ApiRateLimited => "api.rate_limited", "The machine sent too many requests; retry after the time given.";
    ApiOriginRefused => "api.origin_refused", "The request carries a browser Origin or Sec-Fetch-* header whose origin serve.allow_origins doesn't list.";
    ApiDraining => "api.draining", "serve is stopping and takes no new requests; retry once it's back.";
    ApiCheckFailed => "api.check_failed", "The outside check failed or timed out, so nothing was decided; retry.";
    ApiMachineRevoked => "api.machine_revoked", "The machine's key was revoked for this graph.";
    ApiMachinePending => "api.machine_pending", "The machine's enrolment waits for a person to approve it.";
    ApiEnrollPending => "api.enroll_pending", "An enrolment for this machine id already waits for approval.";
    ApiEnrollExists => "api.enroll_exists", "The graph already has a machine with this id, in any state; a revoked machine enrols again with a new identity.";
    ApiEnrollChanged => "api.enroll_changed", "The pending enrolment isn't the one the approval named.";
    ApiClockSkew => "api.clock_skew", "The request's signing time is outside the coordinator's window.";
    ApiReplayed => "api.replayed", "The request's nonce was already used, or it was signed for another boot of serve.";
    ApiGraphUnknown => "api.graph_unknown", "This serve process doesn't serve the graph the request names.";
    ApiActorNotAllowed => "api.actor_not_allowed", "The actor may not make this request from this machine, turn or interface.";
    ApiUnreachable => "api.unreachable", "The bound coordinator couldn't be reached; nothing was written locally.";
    ApiRemoteBound => "api.remote_bound", "This clone is bound to a remote coordinator, and the command runs only on the coordinator's host.";
    ApiRemoteConflict => "api.remote_conflict", "The store's lease mode or the clone's binding conflicts with the request.";
    ApiRemoteAttaching => "api.remote_attaching", "remote attach stopped part way; finish or undo it first.";
    ApiCoordinatorMismatch => "api.coordinator_mismatch", "The coordinator's id differs from the one this clone is bound to.";
    ApiShownMismatch => "api.shown_mismatch", "The turn's shown set differs from the one the coordinator recorded.";
    RunNotPlaced => "run.not_placed", "The agent is placed on another machine, or this machine may not take it.";
    RunMachineLost => "run.machine_lost", "The machine running the turn was marked lost.";
    RunMachineRevoked => "run.machine_revoked", "The machine running the turn was revoked.";
    RunTurnEnded => "run.turn_ended", "The turn has already ended.";
    RunRecoveryFailed => "run.recovery_failed", "A post-turn step couldn't be finished from its recovery record.";
    RunPushRefused => "run.push_refused", "The turn's branch push was refused by the receiving side.";
    RunBadAgentName => "run.bad_agent_name", "An agent name can't end in a .g<k> suffix.";
    CoordCommitUnknown => "coord.commit_unknown", "The coordinator's repository doesn't have the commit on the agent's branch; push it first.";
    CoordBranchNotPushable => "coord.branch_not_pushable", "The worktree isn't on a branch of an agent placed on this machine, so it can't be pushed or settled remotely.";
    ApiKeySkewed => "api.key_skewed", "The idempotency key's timestamp is more than 5 minutes from the coordinator's clock at first use.";
    ApiPersonUnknown => "api.person_unknown", "The person key isn't enrolled in this graph, or was revoked.";
    ApiPersonSignatureRequired => "api.person_signature_required", "A person's request over the network needs a person signature, except reads, own acks and leaving a channel.";
    RunNothingToDo => "run.nothing_to_do", "turn.admit found no candidates, so no turn was admitted and nothing was written.";
    ApiAttestationFailed => "api.attestation_failed", "The key's attestation evidence is present but doesn't verify; approve it at machine grade with --machine-grade, or enrol again.";
    ApiMachineGradeRequired => "api.machine_grade_required", "The key is machine grade; approve with --machine-grade to acknowledge it.";
    // a-flow's R33 code, which serve-store's import check already returns
    // (S29); registered here so every code it returns is one (#10686).
    LedgerBlockInvalid => "ledger.block_invalid", "A turn's ledger block was refused whole and applied as nothing; the fault says why.";

    // The graph ref (edd-01a18867), every lane's codes in one block.
    GraphRefMissing => "graph.ref_missing", "The graph ref doesn't exist; run install to create or adopt it.";
    GraphRefInvalid => "graph.ref_invalid", "The graph ref is symbolic, a protected or tracking branch, the main ref, or not a plain branch.";
    GraphRefExists => "graph.ref_exists", "A request that creates the graph ref found it already there.";
    GraphConfigInvalid => "graph.config_invalid", "A clone-local graph setting in the common git config is malformed.";
    GraphIdentityMismatch => "graph.identity_mismatch", "The graph ref holds another graph than the clone's identity record names.";
    GraphCheckoutMissing => "graph.checkout_missing", "Checkout mode, and this clone used a graph ref but registers no graph checkout; run install.";
    GraphIdentityUnrecorded => "graph.identity_unrecorded", "The clone has no identity record for the graph ref; run install.";
    GraphIdentityUnset => "graph.identity_unset", "No git user identity is configured for graph commits.";
    GraphAuthorityInvalid => "graph.authority_invalid", "The committed config or an agent definition at the graph tip is a link, oversized, or not valid JSON.";
    GraphLockRequired => "graph.lock_required", "The op needs the clone's exclusive graph lock, or the filesystem can't give a working one.";
    GraphWriteInvalid => "graph.write_invalid", "A graph write is malformed: a path outside the graph folders, a bad request id, or a record that doesn't match its commit.";
    GraphWriteStale => "graph.write_stale", "A path the write touches changed on the graph since the request's base.";
    GraphWriteContended => "graph.write_contended", "The graph ref kept moving; the write's retry limit was reached.";
    GraphRequestReused => "graph.request_reused", "A request id was reused with a different payload.";
    GraphGit => "graph.git", "A local git plumbing command for the graph failed.";
    GraphIo => "graph.io", "A graph file under the common git folder couldn't be read or written.";
    GraphCheckoutRead => "graph.checkout_read", "In ref mode, a graph file was read from a code checkout instead of the graph.";
    GraphCacheEdit => "graph.cache_edit", "A graph cache file differed from the graph tip; it was rescued, or the op was refused.";
    GraphSessionDiscarded => "graph.session_discarded", "A write session ended without publishing; its files were restored and the op is unfinished.";
    GraphUnleasedEdit => "graph.unleased_edit", "A write changed a graph path outside its leases or session record.";
    GraphIntentStale => "graph.intent_stale", "A recorded write can't publish because the graph changed a path it touches.";
    GraphIntents => "graph.intents", "A graph write intent is pending replay.";
    GraphSettledElsewhere => "graph.settled_elsewhere", "Another clone settled this node, or last wrote it while this clone holds no ledger for it.";
    GraphImportStale => "graph.import_stale", "An import's preimage differs from the graph's current blob.";
    GraphImportUnfinishedOp => "graph.import_unfinished_op", "The folder holds an unfinished or unclassified op's files, which can't be imported.";
    GraphHandEdits => "graph.hand_edits", "A checkout or the view holds graph changes that differ from the graph.";
    GraphMigrating => "graph.migrating", "A migration to the graph ref is part-way through; resume it with install.";
    GraphOverlappingWorkspace => "graph.overlapping_workspace", "A graph folder is not a dedicated folder, or overlaps another graph folder or code.";
    GraphShape => "graph.shape", "The graph's shape doesn't suit its remote: a full-shape graph on a remote other than the code's.";
    GraphRemoteInvalid => "graph.remote_invalid", "The graph remote or its remote ref is misconfigured.";
    GraphRemoteChanged => "graph.remote_changed", "The graph remote's effective destination differs from the pinned one.";
    GraphOpenChangesets => "graph.open_changesets", "The move to the graph ref waits: changesets are still open, and they settle only on main.";
    GraphUncommitted => "graph.uncommitted", "The move to the graph ref waits: the checkout's graph has uncommitted changes.";
    GraphDiverged => "graph.diverged", "The move to the graph ref waits: the checkout's graph has changes the graph branch doesn't.";
    GraphRemoteIsCode => "graph.remote_is_code", "The graph's remote ref is, or shares history with, a code branch.";
    GraphRemoteRewritten => "graph.remote_rewritten", "The remote graph ref was rewritten relative to the last accepted tip.";
    GraphRemoteUnreachable => "graph.remote_unreachable", "The graph remote couldn't be reached.";
    GraphRemoteAuth => "graph.remote_auth", "The graph remote refused authentication.";
    GraphPushRefused => "graph.push_refused", "A hook refused the graph push; background sync stopped until a person syncs.";
    GraphPushLarge => "graph.push_large", "The outgoing graph push is over the size cap.";
    GraphSecret => "graph.secret", "An outgoing graph blob matches a secret pattern; the push was refused.";
    GraphSecretScanUnavailable => "graph.secret_scan_unavailable", "The secret scanner is unavailable, so graph pushes are refused.";
    GraphSyncConflict => "graph.sync_conflict", "A sync took the remote's version of a path; the local version is recorded as a conflict.";
    GraphSyncContended => "graph.sync_contended", "The graph sync kept losing races and stopped; local writes are unaffected.";
    GraphSyncInTurn => "graph.sync_in_turn", "A graph sync was asked for inside an agent's turn.";
    GraphSyncInvalid => "graph.sync_invalid", "A candidate graph tip failed strict validation and wasn't taken.";
    GraphPublishDisabled => "graph.publish_disabled", "Publishing design is off for this clone.";
    GraphDesignCurrent => "graph.design_current", "The base's design folders already match the graph tip.";
    GraphDesignDrift => "graph.design_drift", "The main ref's design folders differ from the graph tip's.";
    GraphView => "graph.view", "The graph view is checked out on the graph branch, holds hand edits, or is behind.";
    RunAuthorityOverride => "run.authority_override", "A user or local layer changes an authority field of an agent definition.";
    CoordAbandonPublished => "coord.abandon_published", "Abandon refuses a changeset the graph ref already holds.";
    GraphEditPathUnsafe => "graph.edit_path_unsafe", "A link or other unsafe entry is on a node edit copy's path.";
    GraphEditCopyKept => "graph.edit_copy_kept", "A modified node edit copy was kept.";
    GraphEditUnexcluded => "graph.edit_unexcluded", "info/exclude can't be written, so git add -A could take the node edit copy.";
    GraphNothingToSync => "graph.nothing_to_sync", "In ref mode, sync has nothing to bring in.";
    GraphNoCodeBase => "graph.no_code_base", "Neither the main ref nor a work base resolves, so a changed-only check counts nothing as changed.";
    GraphUnsupported => "graph.unsupported", "The command isn't supported in ref mode.";
    GraphWriteNoop => "graph.write_noop", "A graph write's content is what the graph already holds; nothing was written.";
    MsgReservedId => "msg.reserved_id", "Message ids and new conversations beginning `fault.` are reserved to mdkg's own senders.";
    StoreIntegrity => "store.integrity", "The store holds a row its own rules forbid, such as a message under a fault's id that isn't that fault; nothing was taken from it.";
    RunNotLoggedIn => "run.not_logged_in", "The agent's harness reports it isn't logged in, so none of its turns run until it is.";
    RunSupervisorExited => "run.supervisor_exited", "An agent's supervisor exited with an error, or gave up on its startup sync or lease check.";
    RunMailSetAside => "run.mail_set_aside", "Mail shown in three turns that failed before acking it was set aside, acked unanswered; the fault's suggested command shows it, to send again once the agent's turns succeed.";
    RunLoopPaused => "run.loop_paused", "The loop guard paused a thread that used its automatic turns for the hour; it runs again when a person runs the agent or the hour cools down.";
    // The launch wrapper (API v1 A19).
    RunWrapperRefused => "run.wrapper_refused", "The launch wrapper refused the turn (exit 125) before starting the harness; the agent isn't run again.";
    // The base resolver (edd graph-ref-authority, L5b).
    RunNoBase => "run.no_base", "No commit to start a worker's worktree from: the code base is unborn, or no base, mainRef or checkout branch names one.";
    // mdkg flow (edd-01a1878e), a-flow's codes.
    EntryVerdictUnbacked => "entry.verdict_unbacked", "A findings verdict needs a finding the completing reviewer opened, reopened or seconded on the node since the round opened.";
    EntryFixAbsent => "entry.fix_absent", "A verify's SHA undoes the fix: some fix commit's paths are back as they were at its first parent.";
    MsgReviewUnranged => "msg.review_unranged", "A review_request with --node needs --range <base>..<sha>; nothing was sent.";
    MsgReviewMultiNode => "msg.review_multi_node", "A review_request names exactly one node; one naming more is refused whole, with nothing sent.";
    EntryRoundUnanchored => "entry.round_unanchored", "The review round opened unanchored: its walks need an anchored round before it.";
    EntryLedgerUnwritten => "entry.ledger_unwritten", "done wrote no # Ledger section: the node file can't take one that compares equal without it, or would go over index.limits.max_file_bytes with it.";
    // a-flow-p4's requirement lint (A28).
    ReqMissing => "req.missing", "A task, bug or spike has no requirement items (a warning).";
    ReqBadLine => "req.bad_line", "A requirement item doesn't parse as R<n>, or Markdown around it may change what parses (a warning).";
    ReqDuplicateId => "req.duplicate_id", "Two requirement items in one node share an id (a warning).";
    ReqNotEars => "req.not_ears", "A non-spike requirement matches none of the EARS forms (a warning).";
    ReqVague => "req.vague", "A requirement uses a vague word and no number (a warning).";
    ReqNotQuestion => "req.not_question", "A spike's requirement doesn't end with a question mark (a warning).";
    ReqBadRelation => "req.bad_relation", "A requirement's relation names no edd or prd, or a changed requirement the target lacks (a warning).";
    ReqRelationOnDesign => "req.relation_on_design", "A design node's own requirement carries a relation (a warning).";
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
