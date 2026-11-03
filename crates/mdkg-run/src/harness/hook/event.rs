//! The canonical hook event and decision (`mdkg.hook.event.v1`,
//! `mdkg.hook.decision.v1`): what every harness adapter turns its native
//! input into, and what rules decide, with no harness's names in either.
//! The schemas are a contract published in `schemas/`, so tools outside
//! mdkg can produce or consume them.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const EVENT_SCHEMA: &str = "mdkg.hook.event.v1";
pub const DECISION_SCHEMA: &str = "mdkg.hook.decision.v1";

/// A canonical event: what happened, in which harness, by whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookEvent {
    /// Always `mdkg.hook.event.v1`.
    #[schemars(extend("const" = "mdkg.hook.event.v1"))]
    pub schema: String,
    pub event: EventName,
    pub harness: HarnessName,
    /// The harness's session, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The harness's turn, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// The folder the agent works in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub actor: Actor,
    /// The tool, for `tool.before` and `tool.after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<Tool>,
    /// `turn.stop` only: the harness says this stop already follows a block.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stop_active: bool,
    pub native: Native,
}

/// The events of v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum EventName {
    /// A session or turn begins.
    #[serde(rename = "session.start")]
    SessionStart,
    /// A user or launcher prompt arrives.
    #[serde(rename = "prompt.submit")]
    PromptSubmit,
    /// A tool is about to run.
    #[serde(rename = "tool.before")]
    ToolBefore,
    /// A tool finished (0.7.2; no rule uses it yet).
    #[serde(rename = "tool.after")]
    ToolAfter,
    /// The agent is about to stop.
    #[serde(rename = "turn.stop")]
    TurnStop,
    /// An idle session waits for something to do.
    #[serde(rename = "session.idle")]
    SessionIdle,
    /// The git adapter's event, before a commit.
    #[serde(rename = "git.pre-commit")]
    GitPreCommit,
    /// The git adapter's event, before a push.
    #[serde(rename = "git.pre-push")]
    GitPrePush,
    /// The git adapter's event, before a merge that needs no resolving
    /// commits (a conflicted one's commit is `git.pre-commit`).
    #[serde(rename = "git.pre-merge-commit")]
    GitPreMergeCommit,
    /// The git adapter's event, when git has prepared a ref update.
    #[serde(rename = "git.reference-transaction")]
    GitReferenceTransaction,
}

impl EventName {
    #[cfg(test)]
    pub const ALL: [EventName; 10] = [
        EventName::SessionStart,
        EventName::PromptSubmit,
        EventName::ToolBefore,
        EventName::ToolAfter,
        EventName::TurnStop,
        EventName::SessionIdle,
        EventName::GitPreCommit,
        EventName::GitPrePush,
        EventName::GitPreMergeCommit,
        EventName::GitReferenceTransaction,
    ];

    /// Whether only git's own hooks deliver it, never an agent harness.
    pub fn is_git(self) -> bool {
        matches!(
            self,
            EventName::GitPreCommit
                | EventName::GitPrePush
                | EventName::GitPreMergeCommit
                | EventName::GitReferenceTransaction
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EventName::SessionStart => "session.start",
            EventName::PromptSubmit => "prompt.submit",
            EventName::ToolBefore => "tool.before",
            EventName::ToolAfter => "tool.after",
            EventName::TurnStop => "turn.stop",
            EventName::SessionIdle => "session.idle",
            EventName::GitPreCommit => "git.pre-commit",
            EventName::GitPrePush => "git.pre-push",
            EventName::GitPreMergeCommit => "git.pre-merge-commit",
            EventName::GitReferenceTransaction => "git.reference-transaction",
        }
    }
}

macro_rules! hook_harnesses {
    ($(($variant:ident, $id:literal, $adapter:ident)),* $(,)?) => {
        /// The adapters: three agent harnesses, and git's own hooks.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "lowercase")]
        pub enum HarnessName {
            $(#[serde(rename = $id)] $variant,)*
            Git,
        }
        impl HarnessName {
            pub const ALL: [Self; [$(stringify!($variant)),*, "git"].len()] = [$(Self::$variant,)* Self::Git];
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $id,)* Self::Git => "git" }
            }
            pub fn harness(self) -> Option<crate::harness::Harness> {
                match self { $(Self::$variant => Some(crate::harness::Harness::$variant),)* Self::Git => None }
            }
        }
        impl From<crate::harness::Harness> for HarnessName {
            fn from(h: crate::harness::Harness) -> Self {
                match h { $(crate::harness::Harness::$variant => Self::$variant),* }
            }
        }
    };
}
crate::harness_registry!(hook_harnesses);

/// Who an event is for. Rules name the kinds they apply to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Actor {
    /// The mdkg actor, when the session has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: ActorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    /// Any agent session: an `agents up` worker or an interactive one.
    Agent,
    /// A reviewer turn, which loads no project hooks (SEC7).
    Reviewer,
    /// An agent whose definition says it orchestrates.
    Orchestrator,
    /// A commit or push with no agent marker: only the git adapter sees one.
    Human,
}

/// The environment variables an agent harness sets in the shells its
/// agent runs, so a git hook started from one can tell it from a person's
/// shell. Each harness's source is in the a-hooks-core task node:
/// - `MDKG_RUN_TURN`: mdkg's launcher, in every turn it starts;
/// - `CLAUDECODE`: Claude Code, in its Bash tool (2.1.312);
/// - `CODEX_THREAD_ID`: Codex, in each command it runs (0.164 and later);
/// - `OPENCODE`, `OPENCODE_SESSION_ID`: OpenCode 2.1's shell tool;
/// - `AI_AGENT`: Claude Code and OpenCode both set it, and other tools
///   are adopting it as the generic marker.
pub const AGENT_MARKERS: &[&str] = &[
    "MDKG_RUN_TURN",
    "CLAUDECODE",
    "CODEX_THREAD_ID",
    "OPENCODE",
    "OPENCODE_SESSION_ID",
    "AI_AGENT",
];

/// The kind of whoever runs a git hook, from its environment: an agent
/// when any harness's marker is set and nonempty, else a human. A
/// guardrail, not proof: an agent's shell can unset them.
pub fn git_actor_kind(var: impl Fn(&str) -> Option<String>) -> ActorKind {
    if AGENT_MARKERS
        .iter()
        .any(|m| var(m).is_some_and(|v| !v.trim().is_empty()))
    {
        ActorKind::Agent
    } else {
        ActorKind::Human
    }
}

/// The tool an event is about, classified so a rule never needs a
/// harness's tool names.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Tool {
    pub class: ToolClass,
    /// The harness's own name for it.
    #[serde(default)]
    pub name: String,
    /// A shell command's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The paths an edit would write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// What a shell command does with git, when it runs git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitCall>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ToolClass {
    /// A file write.
    Edit,
    /// A command.
    Shell,
    /// A file read.
    Read,
    /// An MCP call.
    Mcp,
    #[default]
    Other,
}

/// A git commit or push a shell command (or the git adapter) makes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct GitCall {
    pub verb: GitVerb,
    /// A force push: `--force`, `-f`, `--force-with-lease`, `--mirror`, or
    /// a refspec starting with `+`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// The remote a push names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// The branches a push updates, as short names; `HEAD` for the current
    /// branch. Empty when the push names none (git's default applies).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    /// `--all` or `--mirror`: every branch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// It skips git's own hooks: `--no-verify` (`-n` for a commit), or
    /// a `core.hooksPath` set for the one command (`-c`, `--config-env`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skips_hooks: bool,
    /// The branches a push names without a destination (`work`, not
    /// `work:work`): the remote's push refspecs can send those elsewhere.
    /// Read by `git.protect`; not part of the event.
    #[serde(skip)]
    #[schemars(skip)]
    pub bare: Vec<String>,
    /// The folder git runs in, from `-C` (each relative to the one
    /// before): where `git.protect` reads the branch checked out and the
    /// config. Not part of the event.
    #[serde(skip)]
    #[schemars(skip)]
    pub at: Option<PathBuf>,
    /// The commits a push sends from, as its refspecs name them (`work`
    /// of `work:main`); empty when it names none. Read by `secret.scan`;
    /// not part of the event.
    #[serde(skip)]
    #[schemars(skip)]
    pub sources: Vec<String>,
    /// What a commit stages beyond the index: a commit's own, or, on a
    /// push, that of a commit the same command line makes first (`git
    /// commit -a && git push`). Read by `secret.scan`; not part of the
    /// event.
    #[serde(skip)]
    #[schemars(skip)]
    pub commit: Option<Stages>,
    /// The folder git runs in can't be read (a `cd` or `-C` the shell
    /// expands, `--git-dir`, `--work-tree`): `secret.scan` refuses rather
    /// than read another folder. Not part of the event.
    #[serde(skip)]
    #[schemars(skip)]
    pub at_unknown: bool,
}

/// What a commit would stage beyond the index, read from its words. (Its
/// whole index is read as staged too: a `--only` pathspec commit leaves
/// the rest staged for the next one.)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stages {
    /// `-a`, or staging the words can't tell (`-p`, `--interactive`,
    /// `--pathspec-from-file`, a pathspec or folder the shell expands):
    /// every tracked change.
    pub all: bool,
    /// The pathspecs: their working-tree changes are staged. Each is read
    /// from the folder the command line started in (a `cd` or `-C` before
    /// it is joined on), or is absolute, or is magic (`:/`).
    pub paths: Vec<String>,
    /// The pathspecs a `git add -f` named: ignored files under these are
    /// staged too, and only under these (review of 161901be).
    pub forced: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GitVerb {
    Commit,
    Push,
    #[default]
    Other,
}

/// The native event the adapter read.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Native {
    pub event: String,
}

/// A canonical decision: what the rules say, and which rules said it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Decision {
    /// Always `mdkg.hook.decision.v1`.
    #[schemars(extend("const" = "mdkg.hook.decision.v1"))]
    pub schema: String,
    pub decision: Verdict,
    /// The reason or context, for anything but `allow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A stable dotted code (`hook.git_protect.denied`), for a
    /// refusal or a warning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codes: Vec<String>,
    /// The rules that matched and said something, in rule order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
}

/// The decisions, least severe first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Silent.
    Allow,
    /// Wake an idle session, with a line.
    Wake,
    /// Text for the agent's turn.
    Context,
    /// Keep the agent from stopping, with a reason.
    BlockStop,
    /// Refuse the tool call, commit or push, with a reason.
    Deny,
}

impl Decision {
    pub fn allow() -> Self {
        Self::new(Verdict::Allow, None)
    }

    pub fn new(decision: Verdict, text: Option<String>) -> Self {
        Self {
            schema: DECISION_SCHEMA.into(),
            decision,
            text,
            codes: Vec::new(),
            rules: Vec::new(),
        }
    }
}

/// What a shell command does with git: its pushes, read as one, before
/// its first `git commit`. Each simple command of a list
/// or pipeline (`&&`, `||`, `;`, `|`, newlines) is read, after any variable
/// assignments and `env`, `command`, `exec` or `sudo`. `None` when it runs
/// no git commit or push that can be read. A `sh -c`, `bash -c`,
/// `zsh -c` or `dash -c` string is read one level deep (so
/// `env -i bash -c 'git push'` is a push); `eval`, a script or a deeper
/// shell isn't, and the git adapter's own hooks catch those. Several
/// pushes are read as one, with every branch each names (see
/// [`merge_pushes`]).
pub fn git_call(command: &str) -> Option<GitCall> {
    let mut all = Vec::new();
    for words in simple_commands(command) {
        match shell_string(&words) {
            Some(inner) => all.extend(simple_commands(&inner)),
            None => all.push(words),
        }
    }
    // A `cd` or `pushd` moves the git commands after it, as `-C` does:
    // read there, or unreadable when its folder can't be (`cd`, `cd -`,
    // `~`, `$`, a backtick, `popd`).
    let mut calls: Vec<GitCall> = Vec::new();
    let mut cd: Option<PathBuf> = None;
    let mut cd_unreadable = false;
    for words in &all {
        if let Some(moved) = change_dir(words) {
            match moved {
                Some(to) => {
                    cd = Some(match cd {
                        Some(prev) => prev.join(to),
                        None => to,
                    })
                }
                None => cd_unreadable = true,
            }
            continue;
        }
        let Some(mut call) = git_words(words) else {
            continue;
        };
        call.all |= cd_unreadable && call.verb == GitVerb::Push;
        call.at_unknown |= cd_unreadable;
        if let Some(dir) = &cd {
            call.at = Some(match call.at.take() {
                Some(at) => dir.join(at),
                None => dir.clone(),
            });
        }
        // A commit's or add's pathspecs, read from where the line started:
        // its own folder joined on (review of 689b8498). A magic one stays
        // as written; see the merge below.
        if let (Some(stages), Some(at)) = (call.commit.as_mut(), call.at.as_ref()) {
            for p in stages.paths.iter_mut().chain(stages.forced.iter_mut()) {
                if !p.starts_with(':') && !Path::new(p.as_str()).is_absolute() {
                    *p = at.join(p.as_str()).to_string_lossy().into_owned();
                }
            }
        }
        calls.push(call);
    }
    // Environment that points git at other config or another repository
    // for the command (`GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n`,
    // `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_GLOBAL`, `GIT_DIR`, `HOME`...),
    // assigned anywhere in it: a push can't be read.
    if all.iter().any(|w| sets_git_environment(w)) {
        for c in calls.iter_mut().filter(|c| c.verb == GitVerb::Push) {
            c.all = true;
        }
    }
    let pushes: Vec<GitCall> = calls
        .iter()
        .filter(|c| c.verb == GitVerb::Push)
        .cloned()
        .collect();
    // What the line's commit takes: its own staging, and every `git add`'s
    // (one after it stages for the next commit, which counts too).
    let staged = calls
        .iter()
        .any(|c| c.verb == GitVerb::Commit)
        .then(|| {
            calls
                .iter()
                .filter_map(|c| c.commit.clone())
                .reduce(|a, b| Stages {
                    all: a.all || b.all,
                    paths: [a.paths, b.paths].concat(),
                    forced: [a.forced, b.forced].concat(),
                })
        })
        .flatten();
    // A magic pathspec is read from the folder git runs in. When the line's
    // staging calls run in different folders, one that isn't relative to
    // the top can't be placed: it becomes the whole tree (`:/`), untracked
    // files included, and every tracked change (reviews of 161901be and
    // e070c96c). That reads more than the line stages, never less.
    let folders: Vec<&Option<PathBuf>> = calls
        .iter()
        .filter(|c| c.commit.is_some())
        .map(|c| &c.at)
        .collect();
    let moved = folders.windows(2).any(|w| w[0] != w[1]);
    let staged = staged.map(|mut s| {
        let placed = |p: &str| !p.starts_with(':') || top_relative(p);
        if moved {
            for p in s.paths.iter_mut().chain(s.forced.iter_mut()) {
                if !placed(p) {
                    *p = ":/".into();
                    s.all = true;
                }
            }
        }
        s
    });
    // Any git call in the line whose folder can't be read leaves the scan
    // unsure where the commit's changes are.
    let at_unknown = calls.iter().any(|c| c.at_unknown);
    let mut call = if pushes.is_empty() {
        calls.into_iter().find(|c| c.verb != GitVerb::Other)?
    } else {
        merge_pushes(pushes)
    };
    call.commit = staged;
    call.at_unknown |= at_unknown;
    Some(call)
}

/// Where a `cd` or `pushd` goes: `None` when it isn't one, `Some(None)`
/// when its folder can't be read (`cd` alone or `cd -`, a `~`, `$` or
/// backtick, `popd`).
fn change_dir(words: &[String]) -> Option<Option<PathBuf>> {
    let program = words.first()?;
    match program.as_str() {
        "cd" | "pushd" => {}
        "popd" => return Some(None),
        _ => return None,
    }
    let target = words[1..].iter().find(|w| !w.starts_with('-') || *w == "-");
    Some(match target {
        Some(t) if t != "-" && !t.contains(['~', '$', '`']) => Some(PathBuf::from(t)),
        _ => None,
    })
}

/// Whether a simple command assigns environment that changes the config
/// or repository git reads: `GIT_CONFIG*`, `GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_COMMON_DIR`, `GIT_NAMESPACE`, `HOME` or `XDG_CONFIG_HOME`, as a
/// word of its own (`NAME=`, `export NAME=`, `env NAME=`, an `env -S`
/// string). Data that only mentions one (`-m 'GIT_CONFIG_COUNT'`) isn't.
fn sets_git_environment(words: &[String]) -> bool {
    let key = |w: &str| w.split_once('=').map(|(name, _)| name.to_string());
    let assignment = |w: &str| {
        key(w).is_some_and(|n| {
            !n.is_empty() && n.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
        })
    };
    let named = |w: &str| {
        assignment(w)
            && key(w).is_some_and(|n| {
                n.starts_with("GIT_CONFIG")
                    || matches!(
                        n.as_str(),
                        "GIT_DIR"
                            | "GIT_WORK_TREE"
                            | "GIT_COMMON_DIR"
                            | "GIT_NAMESPACE"
                            | "HOME"
                            | "XDG_CONFIG_HOME"
                    )
            })
    };
    // Only where a shell or `env` reads an assignment: before the
    // program, among `env`'s words (and its `-S` string), and as
    // `export`/`declare`/`typeset`/`readonly`/`local`'s arguments. An
    // argument that only looks like one (`-m 'GIT_CONFIG_COUNT=1'`) isn't.
    let mut i = 0;
    loop {
        while let Some(w) = words.get(i).filter(|w| assignment(w)) {
            if named(w) {
                return true;
            }
            i += 1;
        }
        let Some(program) = words.get(i) else {
            return false;
        };
        match program.rsplit('/').next().unwrap_or(program) {
            "export" | "declare" | "typeset" | "readonly" | "local" => {
                return words[i + 1..].iter().any(|w| named(w));
            }
            "env" => {
                i += 1;
                while let Some(w) = words.get(i) {
                    let split = match w.as_str() {
                        "-S" | "--split-string" => {
                            i += 1;
                            words.get(i).map(String::as_str)
                        }
                        _ => w
                            .strip_prefix("--split-string=")
                            .or_else(|| w.strip_prefix("-S").filter(|s| !s.is_empty())),
                    };
                    if let Some(string) = split {
                        if string.split_whitespace().any(named) {
                            return true;
                        }
                    } else if !(w.starts_with('-') || assignment(w)) {
                        break;
                    } else if named(w) {
                        return true;
                    }
                    i += 1;
                }
            }
            "sudo" | "command" | "builtin" | "nice" | "timeout" | "nohup" | "exec" | "time" => {
                i += 1;
                while words.get(i).is_some_and(|w| w.starts_with('-')) {
                    i += 1;
                }
            }
            _ => return false,
        }
    }
}

/// Several pushes as one: every branch any names, and any force, `--all`
/// or skipped hooks. Pushes to different remotes, or one naming branches
/// beside one naming none (which goes where the branch checked out
/// does), can't be read as one push: that counts as every branch.
fn merge_pushes(pushes: Vec<GitCall>) -> GitCall {
    let mut it = pushes.into_iter();
    let mut out = it.next().unwrap_or_default();
    for p in it {
        out.all |= p.all
            || p.remote != out.remote
            || p.at != out.at
            || p.branches.is_empty() != out.branches.is_empty();
        out.force |= p.force;
        out.skips_hooks |= p.skips_hooks;
        for b in p.branches {
            if !out.branches.contains(&b) {
                out.branches.push(b);
            }
        }
        for b in p.bare {
            if !out.bare.contains(&b) {
                out.bare.push(b);
            }
        }
        for src in p.sources {
            if !out.sources.contains(&src) {
                out.sources.push(src);
            }
        }
    }
    out
}

/// Git's global options that take a value as the next word.
const GIT_VALUE_OPTIONS: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
    "--super-prefix",
    "--list-cmds",
];

/// Push options that take no value and leave where it goes alone.
const PUSH_KNOWN_OPTIONS: &[&str] = &[
    "tags",
    "follow-tags",
    "no-follow-tags",
    "atomic",
    "no-atomic",
    "dry-run",
    "porcelain",
    "set-upstream",
    "thin",
    "no-thin",
    "quiet",
    "verbose",
    "progress",
    "no-progress",
    "verify",
    "no-signed",
    "no-recurse-submodules",
    "ipv4",
    "ipv6",
    "no-force-with-lease",
    // This takes a value only after `=`.
    "signed",
];

/// Push options that take a value as the next word.
const PUSH_VALUE_OPTIONS: &[&str] = &["--repo", "--receive-pack", "--exec", "-o", "--push-option"];

/// The words of a command after its wrappers: variable assignments, and
/// `env` (with its options), `command`, `exec`, `sudo`, `nohup`, `time`,
/// `nice` and `timeout` with theirs, so `env -i git push` is a push.
/// `env -S <string>` runs its string split into words, so that string is
/// read as the command (with the words after it).
fn unwrap(words: &[String]) -> Vec<String> {
    let mut i = 0;
    let assignment = |w: &str| {
        w.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
        })
    };
    while let Some(w) = words.get(i) {
        let name = w.rsplit('/').next().unwrap_or(w);
        if assignment(w) {
            i += 1;
            continue;
        }
        // A wrapper's options, and how many words each takes after it.
        let options: &[(&str, usize)] = match name {
            "env" => &[("-u", 1), ("--unset", 1), ("-C", 1), ("--chdir", 1)],
            "sudo" => &[
                ("-u", 1),
                ("-g", 1),
                ("-C", 1),
                ("-D", 1),
                ("-h", 1),
                ("-p", 1),
                ("-r", 1),
                ("-t", 1),
                ("-U", 1),
            ],
            "nice" => &[("-n", 1), ("--adjustment", 1)],
            "timeout" => &[("-k", 1), ("--kill-after", 1), ("-s", 1), ("--signal", 1)],
            "command" | "exec" | "nohup" | "time" => &[],
            _ => break,
        };
        i += 1;
        while let Some(o) = words.get(i) {
            if o == "--" {
                i += 1;
                break;
            }
            if name == "env" {
                let split = match o.as_str() {
                    "-S" | "--split-string" => words.get(i + 1).map(|s| (s.as_str(), i + 2)),
                    _ => o
                        .strip_prefix("--split-string=")
                        .or_else(|| o.strip_prefix("-S").filter(|s| !s.is_empty()))
                        .map(|s| (s, i + 1)),
                };
                if let Some((string, after)) = split {
                    let mut command: Vec<String> =
                        simple_commands(string).into_iter().flatten().collect();
                    command.extend(words[after.min(words.len())..].iter().cloned());
                    return unwrap(&command);
                }
            }
            if name == "env" && assignment(o) {
                i += 1;
            } else if let Some((_, n)) = options.iter().find(|(f, _)| f == o) {
                i += 1 + n;
            } else if o.starts_with('-') {
                i += 1;
            } else {
                break;
            }
        }
        // timeout's duration comes before the command.
        if name == "timeout" && words.get(i).is_some() {
            i += 1;
        }
    }
    words[i.min(words.len())..].to_vec()
}

/// The string a `sh`, `bash`, `zsh` or `dash` runs with `-c` (or a bundle
/// holding `c`, like `-lc`), after any wrappers.
fn shell_string(words: &[String]) -> Option<String> {
    let words = unwrap(words);
    let program = words.first()?;
    let name = program.rsplit('/').next().unwrap_or(program);
    if !matches!(name, "sh" | "bash" | "zsh" | "dash") {
        return None;
    }
    // Options (`-e`, `+x`, `-o pipefail`, `-O extglob`, `--norc`) up to
    // `--` or the first word that isn't one; with `-c` among them, that
    // word is the string it runs.
    let mut i = 1;
    let mut c = false;
    while let Some(w) = words.get(i) {
        if w == "--" || w == "-" {
            i += 1;
            break;
        }
        // Long options that take the next word: bash's `--rcfile` and
        // `--init-file`, zsh's `--emulate`.
        if matches!(w.as_str(), "--rcfile" | "--init-file" | "--emulate") {
            i += 2;
            continue;
        }
        if w.starts_with("--") {
            i += 1;
            continue;
        }
        let Some(flags) = w
            .strip_prefix('-')
            .or_else(|| w.strip_prefix('+'))
            .filter(|f| !f.is_empty())
        else {
            break;
        };
        i += 1;
        // A bundle's letters up to `o` or `O`, whose value is the rest of
        // the word (zsh's `-oPIPE_FAIL`) or else the next word.
        for (at, ch) in flags.char_indices() {
            match ch {
                'c' if w.starts_with('-') => c = true,
                'o' | 'O' => {
                    if at + 1 == flags.len() {
                        i += 1;
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    if !c {
        return None;
    }
    words.get(i).cloned()
}

fn git_words(words: &[String]) -> Option<GitCall> {
    let words = unwrap(words);
    let words = words.as_slice();
    let program = words.first()?;
    if program.rsplit('/').next() != Some("git") {
        return None;
    }
    let mut i = 1;
    let mut skips_hooks = false;
    // Config for this one command (`-c`, `--config-env`): a push only
    // passes when its whole command is read with certainty, so any
    // config set for it makes it unreadable (the fail-closed rule).
    let mut push_config = false;
    let hooks_path = |v: &str| v.to_ascii_lowercase().starts_with("core.hookspath");
    let mut at: Option<PathBuf> = None;
    // `--git-dir`, `--work-tree` (and `GIT_DIR`-like spellings): another
    // repository than the folder's, which the tool layer doesn't read.
    let mut elsewhere = false;
    while let Some(w) = words.get(i) {
        if let Some(v) = w
            .strip_prefix("--git-dir=")
            .or_else(|| w.strip_prefix("--work-tree="))
        {
            elsewhere |= !v.is_empty();
            i += 1;
            continue;
        }
        if GIT_VALUE_OPTIONS.contains(&w.as_str()) {
            let value = words.get(i + 1);
            if matches!(w.as_str(), "-c" | "--config-env")
                && let Some(v) = value
            {
                skips_hooks |= hooks_path(v);
                push_config = true;
            }
            if matches!(w.as_str(), "--git-dir" | "--work-tree") {
                elsewhere = true;
            }
            // A `-C` folder the shell expands (`~`, `$`, a backtick) can't
            // be read.
            if w == "-C" && value.is_some_and(|v| v.contains(['~', '$', '`'])) {
                elsewhere = true;
            }
            if w == "-C"
                && let Some(v) = value.filter(|v| !v.is_empty())
            {
                at = Some(match at {
                    Some(prev) => prev.join(v),
                    None => PathBuf::from(v),
                });
            }
            i += 2;
        } else if let Some(v) = w.strip_prefix("--config-env=") {
            skips_hooks |= hooks_path(v);
            push_config = true;
            i += 1;
        } else if w.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let verb = words.get(i)?;
    let args = &words[i + 1..];
    let mut call = match verb.as_str() {
        "commit" => GitCall {
            verb: GitVerb::Commit,
            skips_hooks: args
                .iter()
                .take_while(|a| *a != "--")
                .any(|a| a == "--no-verify" || short_n(a)),
            commit: Some(commit_stages(args)),
            ..GitCall::default()
        },
        "push" => push_call(args),
        // What a commit later in the line would take: read only for that.
        "add" => GitCall {
            commit: Some(add_stages(args)),
            ..GitCall::default()
        },
        _ => return None,
    };
    call.skips_hooks |= skips_hooks;
    // Where it goes is set for this command alone, or in a repository
    // named apart from the folder: it can't be read.
    call.all |= (push_config || elsewhere) && call.verb == GitVerb::Push;
    call.at_unknown |= elsewhere;
    call.at = at;
    Some(call)
}

/// A commit's options that take the next word as their value.
const COMMIT_VALUE_OPTIONS: &[&str] = &[
    "-m",
    "--message",
    "-F",
    "--file",
    "-c",
    "--reedit-message",
    "-C",
    "--reuse-message",
    "--fixup",
    "--squash",
    "--author",
    "--date",
    "-t",
    "--template",
    "--cleanup",
    "--trailer",
];

/// What a commit's words stage beyond the index ([`Stages`]).
fn commit_stages(args: &[String]) -> Stages {
    let mut s = Stages::default();
    let mut paths_only = false;
    let mut i = 0;
    while let Some(w) = args.get(i) {
        i += 1;
        if w == "--" && !paths_only {
            paths_only = true;
            continue;
        }
        if paths_only || !w.starts_with('-') || w == "-" {
            s.all |= w.contains(['$', '`', '~']);
            s.paths.push(w.clone());
            continue;
        }
        match w.as_str() {
            "-a" | "--all" => s.all = true,
            "-p" | "--patch" | "--interactive" => s.all = true,
            w if w.starts_with("--pathspec-from-file") => s.all = true,
            w if COMMIT_VALUE_OPTIONS.contains(&w) => i += 1,
            w if w.starts_with("--") => {}
            w => {
                // A bundle of short options, up to one taking a value
                // (the rest of the word, or the next word).
                for (k, c) in w.char_indices().skip(1) {
                    match c {
                        'a' | 'p' => s.all = true,
                        'm' | 'F' | 'c' | 'C' | 't' => {
                            if k + 1 == w.len() {
                                i += 1;
                            }
                            break;
                        }
                        'S' | 'u' => break,
                        _ => {}
                    }
                }
            }
        }
    }
    s
}

/// Whether a magic pathspec is read from the repository's top: its
/// `top` magic, short (`:/`, `:!/`) or among the long form's words
/// (`:(exclude,top)x`), not just first (review of e070c96c).
fn top_relative(spec: &str) -> bool {
    let Some(rest) = spec.strip_prefix(':') else {
        return false;
    };
    match rest.strip_prefix('(') {
        Some(long) => long
            .split_once(')')
            .is_some_and(|(words, _)| words.split(',').any(|w| w.trim() == "top")),
        None => rest
            .chars()
            .take_while(|c| matches!(c, '/' | '!' | '^'))
            .any(|c| c == '/'),
    }
}

/// What `git add`'s words stage ([`Stages`]): its pathspecs; `-A` with
/// none is the whole tree, untracked files included (`:/`); `-u`, a patch
/// or interactive add, or a pathspec the shell expands, every tracked
/// change.
fn add_stages(args: &[String]) -> Stages {
    let mut s = Stages::default();
    let mut every = false;
    let mut force = false;
    let mut paths_only = false;
    for w in args {
        if w == "--" && !paths_only {
            paths_only = true;
        } else if paths_only || !w.starts_with('-') || w == "-" {
            s.all |= w.contains(['$', '`', '~']);
            s.paths.push(w.clone());
        } else {
            match w.as_str() {
                "-A" | "--all" | "--no-ignore-removal" => every = true,
                "-f" | "--force" => force = true,
                "-u" | "--update" | "-p" | "--patch" | "-i" | "--interactive" | "-e" | "--edit" => {
                    s.all = true
                }
                w if w.starts_with("--pathspec-from-file") => s.all = true,
                w if !w.starts_with("--") && w.contains(['A', 'f']) => {
                    every |= w.contains('A');
                    force |= w.contains('f');
                    s.all |= w.contains(['u', 'p', 'i', 'e']);
                }
                w if !w.starts_with("--") && w.contains(['u', 'p', 'i', 'e']) => s.all = true,
                _ => {}
            }
        }
    }
    if every && s.paths.is_empty() {
        s.paths.push(":/".into());
    }
    if force {
        s.forced = s.paths.clone();
    }
    s
}

/// `-n` in a bundle of a commit's short options, before any that takes
/// a value (`-m`, `-F`, `-c`, `-C`, `-t`, `-S`, `-u`): `-nm x` skips
/// hooks, `-m"wip on"` doesn't.
fn short_n(word: &str) -> bool {
    let Some(flags) = word.strip_prefix('-').filter(|f| !f.starts_with('-')) else {
        return false;
    };
    for c in flags.chars() {
        match c {
            'n' => return true,
            'm' | 'F' | 'c' | 'C' | 't' | 'S' | 'u' => return false,
            _ => {}
        }
    }
    false
}

/// A refspec word git or the shell would expand: a glob (`*`, `?`, `[`)
/// or a brace list (`{a,b}`).
fn unreadable_ref(word: &str) -> bool {
    word.contains(['*', '?', '['])
        || word.split_once('{').is_some_and(|(_, rest)| {
            rest.split_once('}')
                .is_some_and(|(inner, _)| inner.contains(','))
        })
}

fn push_call(args: &[String]) -> GitCall {
    let mut call = GitCall {
        verb: GitVerb::Push,
        ..GitCall::default()
    };
    let mut positional: Vec<&str> = Vec::new();
    let mut repo: Option<String> = None;
    let mut i = 0;
    while let Some(w) = args.get(i) {
        i += 1;
        let w = w.as_str();
        if w == "--" {
            positional.extend(args[i..].iter().map(String::as_str));
            break;
        }
        if let Some(long) = w.strip_prefix("--") {
            let name = long.split_once('=').map_or(long, |(n, _)| n);
            match name {
                "force" | "force-with-lease" | "force-if-includes" | "delete" => call.force = true,
                "no-verify" => call.skips_hooks = true,
                "mirror" => {
                    call.force = true;
                    call.all = true;
                }
                "all" | "branches" => call.all = true,
                // `--repo` names the remote, as a positional one would.
                "repo" => {
                    let value = match long.split_once('=') {
                        Some((_, v)) => Some(v.to_string()),
                        None => {
                            i += 1;
                            args.get(i - 1).cloned()
                        }
                    };
                    repo = value;
                }
                // `--prune` deletes the remote's branches with no local
                // counterpart.
                "prune" => call.force = true,
                // `on-demand` and `only` push the submodules too, whose
                // targets this check doesn't read; `check` and `no` don't.
                "recurse-submodules" => {
                    let mode = long.split_once('=').map(|(_, v)| v);
                    if !matches!(mode, Some("check" | "no")) {
                        call.all = true;
                    }
                }
                _ if PUSH_VALUE_OPTIONS.contains(&format!("--{name}").as_str()) => {
                    if !long.contains('=') {
                        i += 1;
                    }
                }
                _ if PUSH_KNOWN_OPTIONS.contains(&name) => {}
                // git takes an abbreviation (`--rep` for `--repo`) and
                // options mdkg doesn't know: unreadable.
                _ => call.all = true,
            }
        } else if let Some(short) = w.strip_prefix('-').filter(|s| !s.is_empty()) {
            // `-f` forces and `-d` deletes.
            if short.contains('f') || short.contains('d') {
                call.force = true;
            }
            if !short.chars().all(|c| "fduqvno46".contains(c)) {
                call.all = true;
            }
            if short.ends_with('o') {
                i += 1;
            }
        } else {
            positional.push(w);
        }
    }
    // A remote or refspec the shell expands (`$1`, `$b`, `` `cmd` ``),
    // positional or `--repo`'s, can't be read: it counts as every branch.
    let expands = |w: &str| w.contains('$') || w.contains('`');
    if positional.iter().any(|w| expands(w)) || repo.as_deref().is_some_and(expands) {
        call.all = true;
    }
    // `--repo` is the remote when no positional one is given (git's
    // own precedence).
    let (remote, refs) = match positional.split_first() {
        Some((first, rest)) => (Some(first.to_string()), rest.to_vec()),
        None => (repo, Vec::new()),
    };
    call.remote = remote;
    for spec in refs {
        let (forced, spec) = match spec.strip_prefix('+') {
            Some(rest) => (true, rest),
            None => (false, spec),
        };
        // `:dst` deletes dst, which is a force push too.
        call.force |= forced || spec.starts_with(':');
        // A pattern git or the shell expands (`refs/heads/*`, `{a,b}`,
        // `ma?n`, `[m]ain`) can't be read: every branch.
        if unreadable_ref(spec) {
            call.all = true;
        }
        // `src:dst` updates dst; `:dst` deletes it; `src` updates src, or
        // where the remote's push refspecs send it.
        let src = spec.split_once(':').map_or(spec, |(s, _)| s);
        if !src.is_empty() && !call.sources.iter().any(|s| s == src) {
            call.sources.push(src.to_string());
        }
        let dst = spec.rsplit_once(':').map_or(spec, |(_, d)| d);
        let dst = dst.strip_prefix("refs/heads/").unwrap_or(dst);
        if !spec.contains(':') && !call.bare.iter().any(|b| b == dst) {
            call.bare.push(dst.to_string());
        }
        if !dst.is_empty() && !call.branches.iter().any(|b| b == dst) {
            call.branches.push(dst.to_string());
        }
    }
    call
}

/// The simple commands of a shell command line, each as its words: quotes
/// removed, split at `&&`, `||`, `;`, `|`, `&` and newlines outside quotes.
/// Not a shell: substitutions and redirections are read as words.
fn simple_commands(line: &str) -> Vec<Vec<String>> {
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    let end_word = |words: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                if let Some(n) = chars.next()
                    && n != '\n'
                {
                    in_word = true;
                    word.push(n);
                }
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                end_word(&mut words, &mut word, &mut in_word);
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => end_word(&mut words, &mut word, &mut in_word),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut words, &mut word, &mut in_word);
    if !words.is_empty() {
        commands.push(words);
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_commits_words_say_what_it_stages_and_a_pushs_where_it_sends_from() {
        let stages = |c: &str| git_call(c).expect("git").commit;
        let paths = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(stages("git commit -m x"), Some(Stages::default()));
        for all in [
            "git commit -a -m x",
            "git commit -am x",
            "git commit --all -m x",
            "git commit -p",
            "git commit --pathspec-from-file=list",
        ] {
            assert!(stages(all).expect("commit").all, "{all}");
        }
        // A value option's word isn't a path, nor is the `-m` bundle's.
        assert_eq!(
            stages("git commit -m 'a b' --author me -F msg src/a.rs -- -odd"),
            Some(Stages {
                paths: paths(&["src/a.rs", "-odd"]),
                ..Stages::default()
            })
        );
        assert_eq!(
            stages("git commit -qm x b.rs").expect("c").paths,
            paths(&["b.rs"])
        );
        assert!(stages("git commit -m x $FILE").expect("c").all);
        // `git add` in the line stages for its commit; alone it's no call.
        assert_eq!(
            stages("git add src/a.rs new.rs && git commit -m x"),
            Some(Stages {
                paths: paths(&["src/a.rs", "new.rs"]),
                ..Stages::default()
            })
        );
        assert_eq!(
            stages("git add -A && git commit -m x").expect("c").paths,
            paths(&[":/"])
        );
        assert!(stages("git add -u && git commit -m x").expect("c").all);
        assert!(stages("git add -p && git commit -m x").expect("c").all);
        assert_eq!(git_call("git add ."), None);
        assert_eq!(
            git_call("git add . && git push origin work")
                .expect("push")
                .commit,
            None
        );
        // Review of 689b8498: `add -f` stages ignored files; an add's paths
        // are read from where the line started; a folder that can't be
        // read is unknown, never the start.
        assert_eq!(
            stages("git add -f .env && git commit -m x")
                .expect("c")
                .forced,
            paths(&[".env"])
        );
        assert_eq!(
            stages("git add -fA && git commit -m x").expect("c").forced,
            paths(&[":/"])
        );
        // Review of 161901be: `-f` covers only its own add's paths, and a
        // magic pathspec read in another folder than the commit's is all.
        let s = stages("git add -f build/stamp.txt && git add . && git commit -m x").expect("c");
        assert_eq!(s.forced, paths(&["build/stamp.txt"]));
        assert_eq!(s.paths, paths(&["build/stamp.txt", "."]));
        assert!(
            stages("git add ':(glob)*.env' && cd sub && git commit -m x")
                .expect("c")
                .all
        );
        assert!(
            !stages("git add ':(glob)*.env' && git commit -m x")
                .expect("c")
                .all
        );
        assert!(
            !stages("git add :/ && cd sub && git commit -m x")
                .expect("c")
                .all
        );
        // Review of e070c96c: a moved, unplaceable magic pathspec becomes
        // the whole tree for the untracked listing too, forced or not;
        // `top` counts wherever it is among the magic words.
        let moved = stages("git add -f ':(glob)*.env' && cd sub && git commit -m x").expect("c");
        assert!(moved.all);
        assert_eq!(
            (moved.paths, moved.forced),
            (paths(&[":/"]), paths(&[":/"]))
        );
        for top in [":(exclude,top)x", ":(literal,top)x", ":!/x", ":/x"] {
            let line = format!("git add '{top}' && cd sub && git commit -m x");
            let s = stages(&line).expect("c");
            assert!(!s.all, "{top}");
            assert_eq!(s.paths, paths(&[top]), "{top}");
        }
        assert_eq!(
            stages("git -C src add lib.rs && git commit -m x")
                .expect("c")
                .paths,
            paths(&["src/lib.rs"])
        );
        assert_eq!(
            stages("cd src && git add lib.rs && cd .. && git commit -m x lib.rs")
                .expect("c")
                .paths,
            paths(&["src/lib.rs", "src/../lib.rs"])
        );
        for unknown in [
            "cd ~/b && git commit -am x",
            "cd \"$REPO\" && git commit -am x && git push",
            "git --git-dir=/x/.git commit -m x",
            "git -C ~/b add . && git commit -m x",
        ] {
            assert!(git_call(unknown).expect("git").at_unknown, "{unknown}");
        }
        assert!(
            !git_call("cd src && git commit -m x")
                .expect("git")
                .at_unknown
        );
        // A push carries the commit its command line makes first.
        let push = git_call("git commit -a -m x && git push origin work:main").expect("push");
        assert_eq!(push.verb, GitVerb::Push);
        assert!(push.commit.expect("commit").all);
        assert_eq!(push.sources, paths(&["work"]));
        let push = git_call("git push origin +HEAD:main feature :gone").expect("push");
        assert_eq!(push.sources, paths(&["HEAD", "feature"]));
        assert_eq!(push.commit, None);
    }

    fn push(remote: &str, branches: &[&str], force: bool) -> Option<GitCall> {
        Some(GitCall {
            verb: GitVerb::Push,
            force,
            remote: Some(remote.into()),
            branches: branches.iter().map(|b| b.to_string()).collect(),
            all: false,
            skips_hooks: false,
            bare: Vec::new(),
            at: None,
            ..GitCall::default()
        })
    }

    #[test]
    fn a_push_names_its_remote_branches_and_force() {
        let git_call = |c: &str| {
            super::git_call(c).map(|mut g| {
                g.bare.clear();
                g.at = None;
                g.sources.clear();
                g.commit = None;
                g
            })
        };
        assert_eq!(
            git_call("git push origin main"),
            push("origin", &["main"], false)
        );
        assert_eq!(
            git_call("cd /w && git -C . push --set-upstream origin HEAD:refs/heads/main"),
            push("origin", &["main"], false)
        );
        assert_eq!(
            git_call("git push -f origin feat"),
            push("origin", &["feat"], true)
        );
        assert_eq!(
            git_call("git push origin +feat"),
            push("origin", &["feat"], true)
        );
        assert_eq!(
            git_call("git push --force-with-lease=feat:abc origin feat"),
            push("origin", &["feat"], true)
        );
        assert_eq!(
            git_call("GIT_TRACE=1 env git push -uf origin a b:c"),
            push("origin", &["a", "c"], true)
        );
        assert_eq!(
            git_call("git push -o ci.skip origin x"),
            push("origin", &["x"], false)
        );
        let bare = git_call("git push").expect("push");
        assert_eq!((bare.remote, bare.branches.len()), (None, 0));
        let all = git_call("git push --mirror backup").expect("push");
        assert!(all.all && all.force);
        assert!(git_call("git push --all origin").expect("push").all);
        // A push after a commit is the one that counts.
        assert_eq!(
            git_call("git commit -m 'x; y' && git push 'origin' \"ma\"in"),
            push("origin", &["main"], false)
        );
    }

    #[test]
    fn a_commit_is_read_and_anything_else_isnt() {
        assert_eq!(
            git_call("git add -A; /usr/bin/git commit -qm wip").map(|c| c.verb),
            Some(GitVerb::Commit)
        );
        // Wrappers and their options, however `env` is spelled.
        for wrapped in [
            "env -i git push origin main",
            "env -u CLAUDECODE -u AI_AGENT git push origin main",
            "env --unset=CLAUDECODE -- git push origin main",
            "/usr/bin/env A=1 git push origin main",
            "timeout 30 git push origin main",
            "timeout -k 5 30s nice -n 10 git push origin main",
            "sudo -u me git push origin main",
            "env -S 'git push origin main'",
            "env -S 'git push' origin main",
            "env --split-string='git push origin main'",
            "env -S'git push origin main'",
            "env -i -S 'A=1 git push origin main'",
            "env -i bash -c 'git push origin main'",
            "env -i bash -o pipefail -c 'git push origin main'",
            "bash +x -O extglob -c 'git push origin main'",
            "bash -c -- 'git push origin main'",
            "bash --norc -ec 'git push origin main'",
            "env -i zsh -oPIPE_FAIL -c 'git push origin main'",
            "zsh --emulate sh -c 'git push origin main'",
            "bash -co pipefail 'git push origin main'",
            "sh -c 'cd . && git push origin main'",
            "/bin/zsh -lc \"git push origin main\"",
        ] {
            let call = git_call(wrapped).unwrap_or_else(|| panic!("{wrapped}"));
            assert_eq!(call.branches, ["main"], "{wrapped}");
        }
        // Every push is read: a later one to main isn't hidden.
        for several in [
            "git push origin feature; git push origin main",
            "env -i sh -c 'git push origin feature; git push origin main'",
        ] {
            let call = git_call(several).expect("push");
            assert!(call.branches.contains(&"main".to_string()), "{several}");
        }
        // Pushes that can't be read as one, or a branch the shell expands,
        // count as every branch.
        for unreadable in [
            "git push origin feature && git push backup feature",
            "git push origin feature; git push",
            "sh -c 'git push origin \"$1\"' _ main",
            "b=main; git push origin $b",
            "git push origin `echo main`",
            "env -i git push origin {main,feature}",
            "env -i git push origin 'refs/heads/*:refs/heads/*'",
            "git push origin ma?n",
            "git push origin [m]ain",
            "git -c push.default=matching push origin",
            "git -c remote.origin.push=work:main push origin work",
            "git --git-dir=/elsewhere/.git push origin work",
            "env -i GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=remote.origin.push \
             GIT_CONFIG_VALUE_0=refs/heads/work:refs/heads/main git push origin work",
            "GIT_DIR=/elsewhere/.git git push origin work",
            "export GIT_CONFIG_PARAMETERS=x; git push origin work",
            "HOME=/tmp/h git push origin work",
            "git -C ~/repo push origin work",
            "git -C \"$R\" push origin work",
            "env -i sh -c 'git push --repo=\"$1\"' _ mirrored",
            "env -i git push --rep=mirrored",
            "git push --frobnicate origin work",
            "env -i git -c include.path=/tmp/push.cfg push origin work",
            "git push --recurse-submodules=on-demand origin work",
            "git push --recurse-submodules=only origin work",
            "env GIT_DIR=/x git push origin work",
            "env -S 'GIT_CONFIG_COUNT=1 git' push origin work",
            "export GIT_DIR=/x; git push origin work",
            "git -c includeIf.onbranch:work.path=/tmp/p push origin work",
            "git -c user.name=n push origin work",
            "git --config-env=user.name=N push origin work",
            "cd && git push origin work",
            "cd ~/clone && git push origin work",
            "pushd \"$D\"; git push origin work",
            "cd - && git push origin work",
            "git push -x origin work",
            "git --work-tree /elsewhere push origin work",
        ] {
            assert!(git_call(unreadable).expect("push").all, "{unreadable}");
        }
        assert!(
            !git_call("git push origin a; git push origin b")
                .expect("push")
                .all
        );
        // A `cd` moves the push, as `-C` does; a bare `--signed` takes no
        // word, so the remote is the next one.
        let moved = git_call("cd /tmp/clone && git -C sub push publish work").expect("push");
        assert_eq!(moved.at, Some(PathBuf::from("/tmp/clone/sub")));
        assert_eq!(
            git_call("git push --signed mirrored")
                .expect("push")
                .remote
                .as_deref(),
            Some("mirrored")
        );
        // Options mdkg knows, and look-alike names, stay readable.
        for readable in [
            "git push -u origin work",
            "git push --set-upstream -q origin work",
            "git push --tags --atomic origin work",
            "git push -o ci.skip --push-option=x origin work",
            "git push --signed=if-asked origin work",
            "git commit -m 'document GIT_CONFIG_COUNT' && git push origin work:work",
            "grep GIT_CONFIG docs; echo $GIT_CONFIG_GLOBAL; git push origin work",
            "cd . && git push origin work",
            "MYGIT_DIR=x GIT_TRACE=1 git push origin work",
            "git commit -m 'document GIT_CONFIG_COUNT=1' && git push origin work:work",
            "git push --recurse-submodules=check origin work",
        ] {
            assert!(!git_call(readable).expect("push").all, "{readable}");
        }
        // A deletion is a force push, however it's spelled.
        for deleting in [
            "git push origin :feature",
            "git push --delete origin feature",
            "git push -d origin feature",
            "git push --prune origin",
        ] {
            assert!(git_call(deleting).expect("push").force, "{deleting}");
        }
        assert!(!git_call("git push origin feature").expect("push").force);
        // Skipping git's hooks is read, for a push and a commit.
        for skipping in [
            "env -S 'git push --no-verify origin main'",
            "git push --no-verify origin work",
            "git -c core.hooksPath=/dev/null push origin work",
            "git -c core.hookspath=x push",
            "git --config-env=core.hooksPath=H push",
            "git commit --no-verify -m x",
            "git commit -nm x",
        ] {
            assert!(git_call(skipping).expect("git").skips_hooks, "{skipping}");
        }
        for plain in [
            "git push origin work",
            "git commit -m x",
            "git commit -m\"wip on\"",
            "git commit -am none",
            "git -c user.name=n push",
        ] {
            assert!(!git_call(plain).expect("git").skips_hooks, "{plain}");
        }
        for other in [
            "git status",
            "echo git push origin main",
            "grep -r 'git push' .",
            "gitx push origin main",
            "sh -c \"bash -c 'git push origin main'\"",
            "eval 'git push origin main'",
            "",
        ] {
            assert_eq!(git_call(other), None, "{other}");
        }
    }

    #[test]
    fn a_git_hook_tells_agents_from_people_by_their_markers() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(git_actor_kind(env(&[])), ActorKind::Human);
        assert_eq!(
            git_actor_kind(env(&[("CLAUDECODE", " ")])),
            ActorKind::Human
        );
        assert_eq!(git_actor_kind(env(&[("TERM", "xterm")])), ActorKind::Human);
        for m in AGENT_MARKERS {
            let pairs: &'static [(&'static str, &'static str)] =
                Box::leak(vec![(*m, "1")].into_boxed_slice());
            assert_eq!(git_actor_kind(env(pairs)), ActorKind::Agent, "{m}");
        }
    }

    #[test]
    fn the_event_and_decision_round_trip_as_the_design_writes_them() {
        let v = json!({
            "schema": "mdkg.hook.event.v1",
            "event": "tool.before",
            "harness": "claude",
            "session": "s", "turn": "t", "cwd": "/abs/path",
            "actor": {"id": "worker-a", "kind": "agent"},
            "tool": {"class": "shell", "name": "Bash", "command": "git push origin main",
                     "git": {"verb": "push", "remote": "origin", "branches": ["main"]}},
            "native": {"event": "PreToolUse"}
        });
        let e: HookEvent = serde_json::from_value(v.clone()).expect("event");
        assert_eq!(e.event, EventName::ToolBefore);
        assert_eq!(e.actor.kind, ActorKind::Agent);
        assert_eq!(serde_json::to_value(&e).expect("json"), v);
        let mut d = Decision::new(Verdict::BlockStop, Some("why".into()));
        d.rules.push("lease.done".into());
        assert_eq!(
            serde_json::to_value(&d).expect("json"),
            json!({"schema": "mdkg.hook.decision.v1", "decision": "block_stop",
                   "text": "why", "rules": ["lease.done"]})
        );
        assert!(Verdict::Deny > Verdict::BlockStop && Verdict::BlockStop > Verdict::Context);
        assert!(Verdict::Context > Verdict::Allow);
        for name in EventName::ALL {
            assert_eq!(
                serde_json::to_value(name).expect("json"),
                json!(name.as_str())
            );
            assert_eq!(name.is_git(), name.as_str().starts_with("git."), "{name:?}");
        }
        for h in HarnessName::ALL {
            assert_eq!(serde_json::to_value(h).expect("json"), json!(h.as_str()));
        }
    }
}
