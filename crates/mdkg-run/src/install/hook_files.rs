//! The hook projections: each harness's hooks, canonical under
//! `.mdkg/hooks/<harness>/`, projected into the file the harness reads.
//! Claude Code's and Codex's are merged into a settings file the user's
//! own hooks share (Claude's per-user `.claude/settings.local.json`); OpenCode's is a plugin file of its own. Enabling one
//! never trusts it for the harness: each asks its user to review hooks.

use super::{
    Action, Op, Planned, Value, locate, plan_text, pretty, read_object, read_text, unmergeable,
};
use crate::error::RunError;
use crate::projection::Harness;
use serde_json::Map;
use std::path::Path;

pub(super) fn layout(h: Harness) -> crate::harness::HookLayout {
    h.adapter().hook_layout()
}

/// Where Claude's hooks went before dec-01a18ee0 item 11 moved them to
/// its per-user file: Claude Code's shared, usually committed, settings.
pub const LEGACY_CLAUDE_TARGET: &str = ".claude/settings.json";

/// The most read when inspecting an existing hook file without changing it.
pub const HOOK_FILE_CAP: u64 = 1 << 20;

/// `path`'s text when it's a plain file of at most [`HOOK_FILE_CAP`]: what
/// install only looks at to tell whether a hook is installed is read no
/// further, since the repository could make it a link to a device, a FIFO
/// or a huge file (#13434, #13893).
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read as _;
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() <= HOOK_FILE_CAP) {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(HOOK_FILE_CAP + 1).read_to_string(&mut text))
        .ok()?;
    (text.len() as u64 <= HOOK_FILE_CAP).then_some(text)
}

/// The entries of `h`'s canonical fragment in `source`'s graph, read as
/// [`read_capped`] reads.
fn canonical_entries(source: &Path, h: Harness) -> Vec<Value> {
    let path = source.join(layout(h).canonical);
    read_capped(&path)
        .and_then(|t| fragment(source, &path, &t).ok())
        .map(|m| {
            m.values()
                .filter_map(Value::as_array)
                .flatten()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a settings file's `text` holds an entry that runs an mdkg hook:
/// one with any command whose program is an mdkg binary, or any command
/// of a `canonical` entry (an install with `--binary` named another
/// program), beside the user's own commands too (#13893, #14098).
fn holds_mdkg_hooks(text: &str, canonical: &[Value]) -> bool {
    let theirs: Vec<&str> = canonical.iter().flat_map(commands).collect();
    serde_json::from_str::<Value>(text)
        .ok()
        .as_ref()
        .and_then(|v| v.get("hooks"))
        .and_then(Value::as_object)
        .is_some_and(|events| {
            events
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .any(|e| {
                    canonical.contains(e)
                        || commands(e).any(|c| is_mdkg_hook(c) || theirs.contains(&c))
                })
        })
}

/// The commands of a hook entry (`{"hooks": [{"command": …}]}`).
fn commands(entry: &Value) -> impl Iterator<Item = &str> {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("command").and_then(Value::as_str))
}

/// Whether `root`'s [`LEGACY_CLAUDE_TARGET`] still holds an mdkg hook
/// entry ([`holds_mdkg_hooks`], with `source`'s canonical entries). It's
/// only read: install never writes the shared file.
pub fn legacy_claude_hooks(source: &Path, root: &Path) -> bool {
    read_capped(&root.join(LEGACY_CLAUDE_TARGET))
        .is_some_and(|t| holds_mdkg_hooks(&t, &canonical_entries(source, Harness::Claude)))
}

/// A per-user hook file (Claude's `.claude/settings.local.json`) is never
/// written through a link: one leading to the shared settings would put
/// the user's hooks in the committed file (#13434).
fn refuse_personal_link(
    root: &Path,
    l: &crate::harness::HookLayout,
    target: &Path,
) -> Result<(), RunError> {
    if l.personal && std::fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(unmergeable(
            root,
            target,
            "is a link, and install never writes a harness's per-user settings through one",
        )
        .with_hint(format!(
            "replace the link {} with a plain file, then run install again",
            l.target
        )));
    }
    Ok(())
}

fn shipped(h: Harness, bin: &str) -> String {
    h.adapter().hook_project(bin)
}

/// `h`'s canonical hook file, repository-relative, and its shipped text,
/// when `source`'s graph has none, or its OpenCode plugin is an exact
/// previous default: in ref mode it's published to the graph ref before
/// a sync, since the cache is never written.
pub fn missing_canonical(source: &Path, bin: &str, h: Harness) -> Option<(String, String)> {
    let l = layout(h);
    let path = source.join(l.canonical);
    if !path.exists() {
        return Some((l.canonical.to_string(), shipped(h, bin)));
    }
    (h == Harness::OpenCode)
        .then(|| read_capped(&path))
        .flatten()
        .and_then(|text| crate::hooks::opencode_plugin_upgrade(&text))
        .map(|text| (l.canonical.to_string(), text))
}

/// What syncing `h`'s hooks writes: the canonical file when the project
/// has none, then its projection.
/// The canonical file is read from `source`'s graph (the checkout, or in
/// ref mode the graph cache); the target is written in `root`.
pub(super) fn plans(
    source: &Path,
    root: &Path,
    bin: &str,
    h: Harness,
) -> Result<Vec<Planned>, RunError> {
    let l = layout(h);
    let canonical_path = source.join(l.canonical);
    let (text, at) = read_text(source, &canonical_path)?;
    let mut plan = Vec::new();
    let canonical = match text {
        Some(t) => {
            if h == Harness::OpenCode
                && let Some(upgraded) = crate::hooks::opencode_plugin_upgrade(&t)
            {
                if source != root {
                    return Err(unmergeable(
                        source,
                        &canonical_path,
                        "is a previous default on the graph ref, and install never writes the graph cache",
                    )
                    .with_hint(format!("run `{} install --hooks` to publish the updated OpenCode plugin first", mdkg_format::program::BIN)));
                }
                plan.push(plan_text(
                    canonical_path.clone(),
                    at,
                    Some(&t),
                    upgraded.clone(),
                ));
                upgraded
            } else {
                t
            }
        }
        // In ref mode `source` is the graph cache, which install never
        // writes: the caller publishes the shipped file to the graph ref
        // first ([`missing_canonical`]), so finding none here means the
        // tip changed under it.
        None if source != root => {
            return Err(unmergeable(
                source,
                &canonical_path,
                "isn't on the graph ref, and install never writes the graph cache",
            )
            .with_hint("run install again: it publishes the hook file to the graph ref first"));
        }
        None => {
            let t = shipped(h, bin);
            plan.push(plan_text(canonical_path.clone(), at, None, t.clone()));
            t
        }
    };
    plan.push(target_plan(source, root, &l, &canonical_path, canonical)?);
    Ok(plan)
}

/// What syncing `h`'s hooks writes to its target, from `canonical`, the
/// text of the canonical file at `canonical_path`.
fn target_plan(
    source: &Path,
    root: &Path,
    l: &crate::harness::HookLayout,
    canonical_path: &Path,
    canonical: String,
) -> Result<Planned, RunError> {
    let target = root.join(l.target);
    refuse_personal_link(root, l, &target)?;
    if std::fs::metadata(&target).is_ok_and(|meta| meta.is_dir()) {
        return Err(unmergeable(
            root,
            &target,
            "is a directory where a hook file belongs",
        ));
    }
    if l.merged {
        let ours = fragment(source, canonical_path, &canonical)?;
        let (old, obj, at) = read_object(root, &target)?;
        let merged = merge(root, &target, obj, &ours)?;
        Ok(plan_text(
            target,
            at,
            old.as_deref(),
            pretty(&Value::Object(merged)),
        ))
    } else {
        let (old, at) = read_text(root, &target)?;
        Ok(plan_text(target, at, old.as_deref(), canonical))
    }
}

/// Plan a projection from the canonical bytes the graph will contain
/// after publication. The caller must validate this with the complete
/// enabled plan, not as a separate per-hook preflight.
pub(super) fn plans_from_canonical(
    source: &Path,
    root: &Path,
    h: Harness,
    canonical: &str,
) -> Result<Vec<Planned>, RunError> {
    let l = layout(h);
    Ok(vec![target_plan(
        source,
        root,
        &l,
        &source.join(l.canonical),
        canonical.into(),
    )?])
}

/// What disabling `h`'s hooks removes: mdkg's entries from the shared
/// file, which goes once nothing else is left, or the plugin file when
/// it's the canonical one. The canonical file stays.
pub(super) fn removals(source: &Path, root: &Path, h: Harness) -> Result<Vec<Planned>, RunError> {
    let l = layout(h);
    let canonical_path = source.join(l.canonical);
    let canonical = read_text(source, &canonical_path)?.0;
    let target = root.join(l.target);
    refuse_personal_link(root, &l, &target)?;
    if !l.merged {
        let (old, _) = read_text(root, &target)?;
        return match old {
            None => Ok(Vec::new()),
            Some(t)
                if Some(&t) == canonical.as_ref()
                    || (h == Harness::OpenCode
                        && crate::hooks::opencode_plugin_upgrade(&t).as_ref()
                            == canonical.as_ref()
                        && canonical.is_some()) =>
            {
                Ok(vec![Planned {
                    at: locate(root, &target, false)?,
                    path: target,
                    action: Action::Removed,
                    op: Op::Remove,
                }])
            }
            Some(_) => Err(unmergeable(
                root,
                &target,
                &format!(
                    "isn't {}'s plugin, so it may be someone's own: remove it by hand",
                    l.canonical
                ),
            )),
        };
    }
    let ours = match &canonical {
        Some(t) => fragment(source, &canonical_path, t)?,
        None => Map::new(),
    };
    let (old, obj, at) = read_object(root, &target)?;
    let Some(old) = old else {
        return Ok(Vec::new());
    };
    let left = unmerge(root, &target, obj, &ours)?;
    if left.is_empty() {
        return Ok(vec![Planned {
            at: locate(root, &target, false)?,
            path: target,
            action: Action::Removed,
            op: Op::Remove,
        }]);
    }
    let plan = plan_text(target, at, Some(&old), pretty(&Value::Object(left)));
    Ok(if plan.action == Action::Unchanged {
        Vec::new()
    } else {
        vec![plan]
    })
}

/// Whether `h`'s projection is as a sync would leave it: `None`, or what
/// differs.
pub fn status(root: &Path, h: Harness) -> Option<String> {
    let l = layout(h);
    let canonical = if h == Harness::OpenCode {
        read_capped(&root.join(l.canonical))
    } else {
        std::fs::read_to_string(root.join(l.canonical)).ok()
    };
    status_with_canonical(root, h, canonical.as_deref())
}

/// Compare the checkout projection with canonical bytes supplied by the
/// authority reader (the pinned graph tip in ref mode). An absent or
/// unreadable canonical never falls back to a checkout's stale copy.
pub fn status_with_canonical(root: &Path, h: Harness, canonical: Option<&str>) -> Option<String> {
    let l = layout(h);
    let canonical_path = root.join(l.canonical);
    let Some(canonical) = canonical else {
        return Some(format!(
            "hooks:{} is enabled, but {} is missing or unreadable",
            h.name(),
            l.canonical
        ));
    };
    if h == Harness::OpenCode {
        if crate::hooks::opencode_plugin_upgrade(canonical).is_some() {
            return Some(format!(
                "{} is a previous default without 2.2's shell routing; run `{} install --hooks`",
                l.canonical,
                mdkg_format::program::BIN
            ));
        }
        if !crate::hooks::opencode_plugin_is_current(canonical) {
            return Some(format!(
                "{} is customized; shell routing is unverified. Compare its execute.before handler with this build's shipped plugin; adding SHELL_TOOLS alone is insufficient. Then run `{} install --hooks` to project your changes",
                l.canonical,
                mdkg_format::program::BIN
            ));
        }
    }
    let target = root.join(l.target);
    let current = if h == Harness::OpenCode {
        read_capped(&target)
    } else {
        std::fs::read_to_string(&target).ok()
    };
    if h == Harness::OpenCode && current.is_none() {
        return Some(format!(
            "{} must be a readable plain file of at most {HOOK_FILE_CAP} bytes; replace a link or special file with a plain copy of {} before running `{} install`",
            l.target,
            l.canonical,
            mdkg_format::program::BIN
        ));
    }
    let synced = if l.merged {
        let Ok(ours) = fragment(root, &canonical_path, canonical) else {
            return Some(format!("{} isn't a hooks fragment", l.canonical));
        };
        let obj = current
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        merge(root, &target, obj.clone(), &ours).is_ok_and(|m| m == obj)
    } else {
        current.as_deref() == Some(canonical)
    };
    (!synced).then(|| format!("{} doesn't hold {}'s hooks", l.target, l.canonical))
}

/// The canonical fragment's hooks: `{"hooks": {<event>: [<entry>, …]}}`,
/// and nothing else.
fn fragment(root: &Path, path: &Path, text: &str) -> Result<Map<String, Value>, RunError> {
    let bad = || {
        unmergeable(
            root,
            path,
            "isn't a hooks fragment ({\"hooks\": {<event>: [...]}})",
        )
    };
    let v: Value = serde_json::from_str(text).map_err(|_| bad())?;
    let obj = v.as_object().ok_or_else(bad)?;
    if obj.keys().any(|k| k != "hooks") {
        return Err(bad());
    }
    let hooks = match obj.get("hooks") {
        None => return Ok(Map::new()),
        Some(Value::Object(h)) => h.clone(),
        Some(_) => return Err(bad()),
    };
    if hooks.values().any(|e| !e.is_array()) {
        return Err(bad());
    }
    Ok(hooks)
}

/// `obj` with mdkg's hook entries replaced by `ours`, at the end of each
/// event's list; the user's own entries and every other key are kept.
fn merge(
    root: &Path,
    path: &Path,
    obj: Map<String, Value>,
    ours: &Map<String, Value>,
) -> Result<Map<String, Value>, RunError> {
    let mut obj = unmerge(root, path, obj, ours)?;
    if ours.is_empty() {
        return Ok(obj);
    }
    let Value::Object(hooks) = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
    else {
        unreachable!("unmerge refuses a `hooks` that isn't an object");
    };
    for (event, entries) in ours {
        let list = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(list) = list else {
            unreachable!("unmerge refuses an event that isn't a list");
        };
        list.extend(entries.as_array().into_iter().flatten().cloned());
    }
    Ok(obj)
}

/// `obj` without mdkg's hook entries: those equal to one of `ours`, and
/// any whose every command runs an mdkg binary's `hook` command (an older
/// build's, or one whose canonical file is gone). An event or `hooks` left
/// empty goes.
fn unmerge(
    root: &Path,
    path: &Path,
    mut obj: Map<String, Value>,
    ours: &Map<String, Value>,
) -> Result<Map<String, Value>, RunError> {
    let mine: Vec<&Value> = ours
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .collect();
    let Some(hooks) = obj.get_mut("hooks") else {
        return Ok(obj);
    };
    let Value::Object(hooks) = hooks else {
        return Err(unmergeable(
            root,
            path,
            "has a `hooks` that isn't an object",
        ));
    };
    let mut emptied = Vec::new();
    for (event, list) in hooks.iter_mut() {
        let Value::Array(list) = list else {
            return Err(unmergeable(
                root,
                path,
                &format!("has a `hooks.{event}` that isn't a list"),
            ));
        };
        let before = list.len();
        list.retain(|e| !mine.contains(&e) && !runs_mdkg_hook(e));
        if list.is_empty() && before > 0 {
            emptied.push(event.clone());
        }
    }
    for event in emptied {
        hooks.remove(&event);
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    Ok(obj)
}

/// Whether every command of a hook entry (`{"hooks": [{"command": …}]}`)
/// is an mdkg binary's `hook` command: its program's file name starts with
/// `mdkg` and its next word is `hook`.
fn runs_mdkg_hook(entry: &Value) -> bool {
    let Some(commands) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    !commands.is_empty()
        && commands.iter().all(|c| {
            c.get("command")
                .and_then(Value::as_str)
                .is_some_and(is_mdkg_hook)
        })
}

/// A simple command, one program and its words, whose program is an mdkg
/// binary and whose first word is `hook`. A line with anything more (`&&`,
/// a pipe, a redirection, a substitution) may hold the user's own command
/// too, so it's mdkg's only when it's a canonical entry (review of
/// 6af00f9).
fn is_mdkg_hook(command: &str) -> bool {
    let Some(words) = shell_words(command) else {
        return false;
    };
    let mut words = words.into_iter();
    let program = words.next().unwrap_or_default();
    let name = program.rsplit('/').next().unwrap_or_default();
    name.starts_with("mdkg") && words.next().as_deref() == Some("hook")
}

/// The words of a shell line that is one simple command, unquoting `'…'`
/// and `"…"` as a shell would (enough to read a program path `shell_word`
/// quoted). `None` for anything else: an operator, a redirection, a
/// substitution, an expansion, an escape or an unclosed quote.
fn shell_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (None, ';' | '&' | '|' | '<' | '>' | '(' | ')' | '`' | '$' | '\\' | '\n' | '\r')
            | (None, '*' | '?' | '[' | '{' | '}' | '~' | '#' | '!')
            | (Some('"'), '`' | '$' | '\\') => return None,
            (None, ' ' | '\t') => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (Some(q), _) if c == q => quote = None,
            _ => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

/// The hooks git's own adapter (`hooks:git`) routes through mdkg.
pub const GIT_ROUTED: &[&str] = &["pre-commit", "pre-push"];

/// Where `hooks:git` keeps its hooks: a folder of the repository's git
/// common dir, shared by every worktree, that `core.hooksPath` names.
pub fn git_hooks_dir(common: &Path) -> std::path::PathBuf {
    crate::hooks::dir(common).join("git")
}

/// The record of the hooks folder `hooks:git` chains to: the effective
/// `core.hooksPath` before it (from any config scope), empty for git's
/// default (`<common dir>/hooks`).
const GIT_PREVIOUS: &str = "previous-hooks-path";

/// The record of the repository's own `core.hooksPath` before it, which
/// uninstalling puts back: `unset`, or `set <value>`.
const GIT_PREVIOUS_LOCAL: &str = "previous-local-hooks-path";

/// The record of the mdkg binary the routed hooks run.
const GIT_BINARY: &str = "mdkg-binary";

/// The record of the worktree whose install last wrote `hooks:git`: the
/// folder is the repository's, so only that worktree's undo removes it.
const GIT_INSTALLED_FROM: &str = "installed-from";

fn canonical(p: &Path) -> String {
    std::fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// A value for a single-quoted shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The shell that resolves the previous hooks folder at run time: git's
/// default, or a relative `core.hooksPath` from the worktree's top, where
/// git runs hooks.
const GIT_PREVIOUS_SH: &str = r#"if [ -z "$prev" ]; then prev="$(git rev-parse --git-common-dir)/hooks"; fi
case "$prev" in /*) ;; *) prev="$(pwd)/$prev" ;; esac
"#;

/// One of `hooks:git`'s hooks. A routed hook runs mdkg's rules first, on
/// its own copy of git's input, and stops git only when mdkg refuses
/// (exit 1): a missing or failing mdkg, or no temporary file for the copy,
/// warns and goes on, so it never breaks a commit. A copy that fails
/// partway stops git, since neither mdkg nor the previous hook would see
/// all of git's input. Every hook then runs the previous folder's own.
pub fn git_hook_script(name: &str, bin: &str, previous: &str) -> String {
    let mut s = format!(
        "#!/bin/sh\n# Written by mdkg (hooks:git). It runs mdkg's rules for this hook, then\n\
         # the hook of the folder core.hooksPath named before, so no hook stops running.\n\
         name={}\nprev={}\n{GIT_PREVIOUS_SH}",
        sh_quote(name),
        sh_quote(previous)
    );
    if GIT_ROUTED.contains(&name) {
        s.push_str(&format!(
            r#"if ! input="$(mktemp "${{TMPDIR:-/tmp}}/mdkg-git-hook.XXXXXX")"; then
  echo "mdkg's $name hook couldn't run (no temporary file); going on" >&2
  if [ -x "$prev/$name" ]; then exec "$prev/$name" "$@"; fi
  exit 0
fi
trap 'rm -f "$input"' EXIT
if ! cat > "$input"; then
  echo "mdkg's $name hook couldn't copy git's input, so no hook would see all of it; stopping" >&2
  exit 1
fi
{} hooks run --harness git --native "$name" -- "$@" < "$input"
status=$?
if [ "$status" -eq 1 ]; then exit 1; fi
if [ "$status" -ne 0 ]; then echo "mdkg's $name hook didn't run (exit $status); going on" >&2; fi
if [ -x "$prev/$name" ]; then "$prev/$name" "$@" < "$input"; exit $?; fi
exit 0
"#,
            sh_quote(bin)
        ));
    } else {
        s.push_str("if [ -x \"$prev/$name\" ]; then exec \"$prev/$name\" \"$@\"; fi\nexit 0\n");
    }
    s
}

fn git_failed(what: String) -> RunError {
    RunError::failure("install.git_hooks", what)
}

/// `git config` with `scope` (`--file <path>`, or nothing for every scope
/// as `top` sees them); `None` when the key isn't set.
fn git_config_at(
    top: &Path,
    file: Option<&Path>,
    args: &[&str],
) -> Result<Option<String>, RunError> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(top).arg("config");
    if let Some(f) = file {
        cmd.arg("--file").arg(f);
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| RunError::io("can't run git config", e))?;
    match out.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\n', '\r'])
                .to_string(),
        )),
        // Not set (`--get`, `--unset`).
        Some(1) | Some(5) => Ok(None),
        _ => Err(git_failed(format!("git config {} failed", args.join(" ")))),
    }
}

/// The `core.hooksPath` git uses in `top`, as a path, with the scope it
/// comes from (`local`, `global`, `worktree`...).
fn effective_hooks_path(top: &Path) -> Result<Option<(String, String)>, RunError> {
    let got = git_config_at(
        top,
        None,
        &["--show-scope", "--type=path", "--get", "core.hooksPath"],
    )?;
    Ok(got.and_then(|l| {
        let (scope, value) = l.split_once('\t')?;
        Some((scope.to_string(), value.to_string()))
    }))
}

/// The repository's own config file's value.
fn local_config(top: &Path, common: &Path, args: &[&str]) -> Result<Option<String>, RunError> {
    git_config_at(top, Some(&common.join("config")), args)
}

fn read_record(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim_end_matches('\n').to_string())
}

/// What `hooks:git` chains to and restores: the effective `core.hooksPath`
/// and the repository's own, as they were before it. A value that isn't
/// mdkg's folder is the current one (another tool may have set it since);
/// else the recorded one.
fn git_previous(top: &Path, common: &Path) -> Result<(String, Option<String>), RunError> {
    let ours = git_hooks_dir(common);
    let is_ours = |v: &Option<String>| v.as_deref().map(Path::new) == Some(ours.as_path());
    let local = local_config(top, common, &["--get", "core.hooksPath"])?;
    // As a path, the way git reads it: `~/.githooks` expanded.
    let effective = git_config_at(top, None, &["--type=path", "--get", "core.hooksPath"])?;
    let chain = if is_ours(&effective) {
        read_record(&ours, GIT_PREVIOUS).unwrap_or_default()
    } else {
        effective.unwrap_or_default()
    };
    let restore = if is_ours(&local) {
        match read_record(&ours, GIT_PREVIOUS_LOCAL).as_deref() {
            Some(r) => r.strip_prefix("set ").map(String::from),
            None => None,
        }
    } else {
        local
    };
    Ok((chain, restore))
}

/// The hooks `hooks:git` writes: mdkg's routed ones, and one for each
/// runnable hook the previous folder has (a link to one too; `top`
/// resolves a relative folder), so they keep running. A hook added there
/// later needs another install.
fn git_names(common: &Path, top: &Path, previous: &str) -> Vec<String> {
    let folder = if previous.is_empty() {
        common.join("hooks")
    } else if Path::new(previous).is_absolute() {
        previous.into()
    } else {
        top.join(previous)
    };
    let mut names: Vec<String> = GIT_ROUTED.iter().map(|s| s.to_string()).collect();
    if let Ok(entries) = std::fs::read_dir(folder) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let runnable = std::fs::metadata(e.path()).is_ok_and(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            });
            if runnable
                && !name.starts_with('-')
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

/// The folder for `hooks:git`, made level by level under the git common
/// dir, refusing any level that's a link, so no write can land elsewhere.
fn git_dir_checked(common: &Path, create: bool) -> Result<std::path::PathBuf, RunError> {
    let dir = git_hooks_dir(common);
    let mut at = common.to_path_buf();
    for part in dir.strip_prefix(common).unwrap_or(&dir).components() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {
                // An earlier install may have made it and failed to sync
                // its entry: sync it now too, so a retry is as durable.
                if create {
                    sync_parent(&at)?;
                }
            }
            Ok(_) => {
                return Err(git_failed(format!(
                    "{} isn't a plain folder; move it aside and install again",
                    at.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if create {
                    std::fs::create_dir(&at)
                        .map_err(|e| RunError::io("can't create the git hooks folder", e))?;
                    // Its entry in the parent too, so a crash never keeps
                    // core.hooksPath but loses the folder.
                    sync_parent(&at)?;
                }
            }
            Err(e) => return Err(RunError::io("can't inspect the git hooks folder", e)),
        }
    }
    Ok(dir)
}

fn sync_parent(at: &Path) -> Result<(), RunError> {
    match at.parent() {
        Some(parent) => std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| RunError::io("can't sync the git hooks folder", e)),
        None => Ok(()),
    }
}

/// The repository's worktrees whose own config (`config.worktree`, read
/// when `extensions.worktreeConfig` is on, with what it includes) sets
/// `core.hooksPath`: each beats the repository's there, so mdkg's hooks
/// would never run in it. Each is asked in its own context, since an
/// `includeIf` (`onbranch:`, `gitdir:`) can differ by worktree.
fn worktree_overrides(top: &Path) -> Result<Vec<std::path::PathBuf>, RunError> {
    let on = git_config_at(
        top,
        None,
        &["--type=bool", "--get", "extensions.worktreeConfig"],
    )?;
    if on.as_deref() != Some("true") {
        return Ok(Vec::new());
    }
    let mut tops = mdkg_coord_sqlite::git::worktrees(top)
        .map_err(|e| RunError::io("can't list the repository's worktrees", e))?;
    tops.push(top.to_path_buf());
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    for t in tops {
        if !t.is_dir() || found.iter().any(|f| f == &t) {
            continue;
        }
        if let Some((scope, _)) = effective_hooks_path(&t)?
            && scope == "worktree"
        {
            found.push(t);
        }
    }
    Ok(found)
}

/// Writes `name` in `dir` whole or not at all: a new file of its own (never
/// through a link), given its mode, then renamed over the old one.
fn write_checked(dir: &Path, name: &str, text: &str, mode: u32) -> Result<(), RunError> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| RunError::io("can't write a git hook", e))?;
    let written = f
        .write_all(text.as_bytes())
        .and_then(|()| f.set_permissions(std::fs::Permissions::from_mode(mode)))
        .and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, dir.join(name))) {
        let _ = std::fs::remove_file(&tmp);
        return Err(RunError::io("can't write a git hook", e));
    }
    // The new name too, so a crash never keeps core.hooksPath but loses a
    // record written before it.
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| RunError::io("can't sync the git hooks folder", e))
}

struct GitInstall {
    top: std::path::PathBuf,
    common: std::path::PathBuf,
    previous: String,
    local: Option<String>,
}

/// Resolve the install's read-only inputs and refusals. Both the future
/// projection preview and the real write must reject worktree overrides.
fn git_install_inputs(root: &Path) -> Result<GitInstall, RunError> {
    let top = mdkg_coord_sqlite::git::toplevel(root)
        .map_err(|e| RunError::io("can't find the worktree's top", e))?;
    let common = mdkg_coord_sqlite::git::common_dir(&top)
        .map_err(|e| RunError::io("can't find the git common dir", e))?;
    // A worktree's own config (`extensions.worktreeConfig`) beats the
    // repository's, so mdkg's hooks would never run in that worktree:
    // this one, or any other of the repository's.
    let overrides = worktree_overrides(&top)?;
    if !overrides.is_empty() {
        let worktrees: Vec<String> = overrides.iter().map(|f| f.display().to_string()).collect();
        return Err(git_failed(format!(
            "a worktree's own config (config.worktree) sets core.hooksPath, which beats the \
             repository's, so mdkg's git hooks would never run there: in {}; unset it in each \
             (`git config --worktree --unset core.hooksPath` in that worktree) and install again",
            worktrees.join(", ")
        )));
    }
    let (previous, local) = git_previous(&top, &common)?;
    Ok(GitInstall {
        top,
        common,
        previous,
        local,
    })
}

fn git_destinations_checked(inputs: &GitInstall) -> Result<(), RunError> {
    let dir = git_dir_checked(&inputs.common, false)?;
    let records = [
        GIT_PREVIOUS,
        GIT_PREVIOUS_LOCAL,
        GIT_BINARY,
        GIT_INSTALLED_FROM,
    ];
    let scripts = git_names(&inputs.common, &inputs.top, &inputs.previous);
    if let Some(name) = scripts.iter().find(|name| records.contains(&name.as_str())) {
        return Err(git_failed(format!(
            "the previous hooks folder has an executable named {name}, which is reserved for mdkg's install records"
        )).with_hint(format!("rename {name} in the previous hooks folder, then install again")));
    }
    for name in records.into_iter().map(String::from).chain(scripts) {
        let path = dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(git_failed(format!(
                    "{} isn't a plain file; move it aside and install again",
                    path.display()
                )));
            }
            Err(e) => return Err(RunError::io("can't inspect a git hook destination", e)),
        }
    }
    Ok(())
}

/// Validate without making the hooks folder or changing configuration.
/// Setup callers use this before creating or adopting a graph ref too.
pub fn git_install_preflight(root: &Path) -> Result<(), RunError> {
    git_destinations_checked(&git_install_inputs(root)?)
}

/// Resolve links that exist, keeping absent suffixes for a future plan.
fn future_path(path: &Path) -> Result<std::path::PathBuf, RunError> {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => {
                out.push(part.as_os_str());
                match std::fs::symlink_metadata(&out) {
                    Ok(_) => {
                        out = std::fs::canonicalize(&out)
                            .map_err(|e| RunError::io("can't resolve a git hooks path", e))?
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(RunError::io("can't inspect a git hooks path", e)),
                }
            }
        }
    }
    Ok(out)
}

/// A projection cannot change the folder Git's chained hook names come
/// from between preview and install. Refuse the overlap before either.
pub(super) fn git_projection_preflight(
    root: &Path,
    destinations: &[std::path::PathBuf],
) -> Result<(), RunError> {
    let inputs = git_install_inputs(root)?;
    git_destinations_checked(&inputs)?;
    let effective = effective_hooks_path(&inputs.top)?
        .map(|(_, value)| value)
        .unwrap_or_default();
    for name in [&effective, &inputs.previous] {
        let path = if name.is_empty() {
            inputs.common.join("hooks")
        } else {
            inputs.top.join(name)
        };
        let resolved = future_path(&path)?;
        for destination in destinations {
            let projected = future_path(destination)?;
            #[cfg(target_os = "macos")]
            let overlaps = {
                use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
                let fold = |p: &Path| {
                    std::path::PathBuf::from(std::ffi::OsString::from_vec(
                        p.as_os_str().as_bytes().to_ascii_lowercase(),
                    ))
                };
                // Absent suffixes cannot be canonicalized yet on APFS.
                fold(&resolved).starts_with(fold(&projected))
            };
            #[cfg(not(target_os = "macos"))]
            let overlaps = resolved.starts_with(&projected);
            if overlaps {
                return Err(git_failed(format!(
                    "git hooks path {} lies inside projected destination {}; install refuses an overlapping hooks path",
                    path.display(), destination.display()
                )).with_hint("move core.hooksPath (and its chained hooks folder) outside the projected destinations"));
            }
        }
    }
    Ok(())
}

/// Installs `hooks:git`: writes its hooks and points the repository's
/// `core.hooksPath` at them. It chains to the folder that was in effect
/// (from any config scope), and records the repository's own value for
/// uninstalling. Never replaces or moves a hook. Run again, it rewrites
/// its own, and follows a folder another tool has set since.
pub fn git_install(root: &Path, bin: &str) -> Result<(), RunError> {
    let inputs = git_install_inputs(root)?;
    git_destinations_checked(&inputs)?;
    let GitInstall {
        top,
        common,
        previous,
        local,
    } = inputs;
    let dir = git_dir_checked(&common, true)?;
    // The records first: a failure after them leaves what to restore.
    write_checked(&dir, GIT_PREVIOUS, &format!("{previous}\n"), 0o644)?;
    let local_record = match &local {
        Some(v) => format!("set {v}\n"),
        None => "unset\n".into(),
    };
    write_checked(&dir, GIT_PREVIOUS_LOCAL, &local_record, 0o644)?;
    write_checked(&dir, GIT_BINARY, &format!("{bin}\n"), 0o644)?;
    write_checked(
        &dir,
        GIT_INSTALLED_FROM,
        &format!("{}\n", canonical(&top)),
        0o644,
    )?;
    for name in git_names(&common, &top, &previous) {
        write_checked(&dir, &name, &git_hook_script(&name, bin, &previous), 0o755)?;
    }
    local_config(&top, &common, &["core.hooksPath", &dir.to_string_lossy()])?;
    Ok(())
}

/// Removes `hooks:git`: the repository's `core.hooksPath` back to what it
/// was (unset when it had none) when it still names mdkg's folder, and the
/// folder gone. The repository's own hooks were never touched.
pub fn git_uninstall(root: &Path) -> Result<(), RunError> {
    let Ok(top) = mdkg_coord_sqlite::git::toplevel(root) else {
        // Not a git checkout: there's nothing of git's to undo.
        return Ok(());
    };
    let common = mdkg_coord_sqlite::git::common_dir(&top)
        .map_err(|e| RunError::io("can't find the git common dir", e))?;
    let dir = git_hooks_dir(&common);
    if std::fs::symlink_metadata(&dir).is_err() {
        return Ok(());
    }
    let (_, restore) = git_previous(&top, &common)?;
    let current = local_config(&top, &common, &["--get", "core.hooksPath"])?;
    if current.as_deref().map(Path::new) == Some(dir.as_path()) {
        match &restore {
            None => local_config(&top, &common, &["--unset", "core.hooksPath"])?,
            Some(v) => local_config(&top, &common, &["core.hooksPath", v])?,
        };
    }
    if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| RunError::io("can't remove the git hooks folder", e))?;
    }
    Ok(())
}

/// Whether this worktree's install last wrote `hooks:git` (an older
/// folder, with no record of it, counts as anyone's).
pub fn git_installed_here(root: &Path) -> bool {
    let Ok(top) = mdkg_coord_sqlite::git::toplevel(root) else {
        return false;
    };
    let Ok(common) = mdkg_coord_sqlite::git::common_dir(&top) else {
        return false;
    };
    let dir = git_hooks_dir(&common);
    if std::fs::symlink_metadata(&dir).is_err() {
        return false;
    }
    read_record(&dir, GIT_INSTALLED_FROM).is_none_or(|from| from == canonical(&top))
}

/// Whether `hooks:git` is installed as an install would leave it: `None`,
/// or what differs.
pub fn git_status(root: &Path) -> Option<String> {
    let top = mdkg_coord_sqlite::git::toplevel(root).ok()?;
    let common = mdkg_coord_sqlite::git::common_dir(&top).ok()?;
    let dir = git_hooks_dir(&common);
    // The value git uses here: a worktree's own config, or a global one
    // when the repository has none, can hide the repository's.
    let current = effective_hooks_path(&top).ok().flatten();
    if current.as_ref().map(|(_, v)| Path::new(v)) != Some(dir.as_path()) {
        let from = current.map_or(String::new(), |(scope, _)| format!(" (its {scope} config)"));
        return Some(format!(
            "hooks:git is enabled, but core.hooksPath here{from} doesn't name mdkg's git hooks"
        ));
    }
    if let Ok(o) = worktree_overrides(&top)
        && !o.is_empty()
    {
        return Some(format!(
            "hooks:git is enabled, but the own config of {} worktree(s) sets core.hooksPath \
             (config.worktree), so mdkg's git hooks don't run in them",
            o.len()
        ));
    }
    let previous = read_record(&dir, GIT_PREVIOUS)?;
    let bin = read_record(&dir, GIT_BINARY)?;
    let stale = git_names(&common, &top, &previous).into_iter().any(|name| {
        read_record(&dir, &name).map(|t| format!("{t}\n"))
            != Some(git_hook_script(&name, &bin, &previous))
    });
    stale.then(|| "hooks:git's hooks differ from what an install writes".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct CodexFixture(std::path::PathBuf);
    impl CodexFixture {
        fn new() -> Self {
            let p = std::env::temp_dir()
                .join(format!("mdkg-codex-projection-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(p.join("repo/.mdkg")).expect("fixture");
            std::fs::write(p.join("repo/.mdkg/config.json"), "{\"projections\": []}\n")
                .expect("config");
            Self(p)
        }
        fn root(&self) -> std::path::PathBuf {
            self.0.join("repo")
        }
    }
    impl Drop for CodexFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn object(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("file")).expect("JSON")
    }

    #[cfg(unix)]
    #[test]
    fn opencode_status_refuses_non_plain_and_oversized_files_without_reading_them() {
        let f = CodexFixture::new();
        let root = f.root();
        let l = layout(Harness::OpenCode);
        let canonical = root.join(l.canonical);
        let target = root.join(l.target);
        let shipped = shipped(Harness::OpenCode, "mdkg");
        std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&canonical, &shipped).unwrap();
        std::fs::write(&target, vec![b' '; HOOK_FILE_CAP as usize + 1]).unwrap();
        assert!(status(&root, Harness::OpenCode).is_some());
        std::fs::remove_file(&target).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&target)
                .status()
                .unwrap()
                .success()
        );
        assert!(status(&root, Harness::OpenCode).is_some());
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &target).unwrap();
        let warning = status(&root, Harness::OpenCode).unwrap();
        assert!(warning.contains("plain copy"), "{warning}");
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, &shipped).unwrap();
        assert_eq!(status(&root, Harness::OpenCode), None);
        std::fs::remove_file(&canonical).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &canonical).unwrap();
        assert!(
            status(&root, Harness::OpenCode)
                .unwrap()
                .contains("missing or unreadable")
        );
    }

    #[test]
    fn pending_canonical_checks_refuse_hard_linked_targets_for_each_harness() {
        use std::fs;
        let f = CodexFixture::new();
        let r = f.root();
        for h in Harness::ALL {
            let l = layout(h);
            let target = r.join(l.target);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            let other = r.join(format!("other-{}.txt", h.name()));
            fs::write(&other, "{}\n").unwrap();
            fs::hard_link(&other, &target).unwrap();
            let projection = crate::projection::name(&format!("hooks:{}", h.name())).unwrap();
            let projections = crate::projection::Projections::of(&[projection]);
            let pending = [(h, shipped(h, "mdkg"))];
            let preview = || {
                super::super::preview_listed_with_hooks(
                    &r,
                    &r,
                    &crate::install::DEFAULT_SERVER,
                    &projections,
                    &[],
                    &|_| true,
                    &pending,
                    None,
                )
            };
            let err = preview().unwrap_err();
            assert!(err.message.contains("hard links"), "{h:?}: {err:?}");
            assert_eq!(fs::read_to_string(&other).unwrap(), "{}\n");
            assert_eq!(fs::read_to_string(&target).unwrap(), "{}\n");
            fs::remove_file(&target).unwrap();
            fs::create_dir(&target).unwrap();
            let err = preview().unwrap_err();
            assert!(err.message.contains("directory"), "{h:?}: {err:?}");
            assert!(target.is_dir());
        }
    }

    #[test]
    fn opencode_upgrades_only_previous_defaults_and_reports_custom_routing() {
        use crate::install::{DEFAULT_SERVER, install_changing_from};
        use std::fs;
        let f = CodexFixture::new();
        let r = f.root();
        let l = layout(Harness::OpenCode);
        let canonical = r.join(l.canonical);
        let target = r.join(l.target);
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let current = crate::hooks::opencode_plugin("/opt/mdkg-original");
        let old = current.replace("[\"bash\", \"shell\"]", "[\"bash\"]");
        fs::write(&canonical, &old).unwrap();
        fs::write(&target, &old).unwrap();
        fs::write(
            r.join(".mdkg/config.json"),
            "{\"projections\":[\"hooks:opencode\"]}",
        )
        .unwrap();
        assert!(
            status(&r, Harness::OpenCode)
                .unwrap()
                .contains("run `mdkg install --hooks`")
        );
        assert_eq!(
            missing_canonical(&r, "new-binary", Harness::OpenCode),
            Some((l.canonical.into(), current.clone()))
        );
        let install = |hooks, dry_run| {
            install_changing_from(&r, &r, &DEFAULT_SERVER, &[], &[], hooks, dry_run, None).unwrap()
        };
        let dry = install(Some(true), true);
        for path in [l.canonical, l.target] {
            assert!(
                dry.changes
                    .iter()
                    .any(|c| c.path == path && c.action == Action::Updated)
            );
        }
        assert_eq!(fs::read_to_string(&canonical).unwrap(), old);
        // No --hooks means even a recognized previous default stays put.
        install(Some(false), false);
        assert_eq!(fs::read_to_string(&canonical).unwrap(), old);
        assert_eq!(fs::read_to_string(&target).unwrap(), old);
        install(Some(true), false);
        assert_eq!(fs::read_to_string(&canonical).unwrap(), current);
        assert_eq!(fs::read_to_string(&target).unwrap(), current);
        assert_eq!(status(&r, Harness::OpenCode), None);
        assert_eq!(missing_canonical(&r, "new-binary", Harness::OpenCode), None);
        // The graph-ref canonical may be current while the checkout
        // still has its previous copy. Never fall back to that copy.
        fs::write(&canonical, &old).unwrap();
        assert!(status(&r, Harness::OpenCode).is_some());
        assert_eq!(
            status_with_canonical(&r, Harness::OpenCode, Some(&current)),
            None
        );
        assert!(status_with_canonical(&r, Harness::OpenCode, None).is_some());
        let custom = old.clone() + "// custom plugin\n";
        fs::write(&canonical, &custom).unwrap();
        fs::write(&target, &custom).unwrap();
        assert!(
            status(&r, Harness::OpenCode)
                .unwrap()
                .contains("customized")
        );
        assert_eq!(missing_canonical(&r, "new-binary", Harness::OpenCode), None);
        install(Some(true), false);
        assert_eq!(fs::read_to_string(&canonical).unwrap(), custom);
        assert_eq!(fs::read_to_string(&target).unwrap(), custom);
        let custom_current = current.clone() + "// custom plugin\n";
        fs::write(&canonical, &custom_current).unwrap();
        fs::write(&target, &custom_current).unwrap();
        assert!(
            status(&r, Harness::OpenCode)
                .unwrap()
                .contains("unverified")
        );
        // Another checkout may have upgraded the shared canonical while
        // this checkout's projection is still an exact previous default.
        fs::write(&canonical, &current).unwrap();
        fs::write(&target, &old).unwrap();
        install_changing_from(
            &r,
            &r,
            &DEFAULT_SERVER,
            &[],
            &["hooks:opencode"],
            Some(true),
            false,
            None,
        )
        .unwrap();
        assert!(!target.exists());
        assert_eq!(fs::read_to_string(&canonical).unwrap(), current);
    }

    #[test]
    fn codex_enable_creates_canonical_merges_user_hooks_and_preserves_trust() {
        use crate::install::{Server, install_changing};
        use std::fs;
        let f = CodexFixture::new();
        let r = f.root();
        fs::create_dir_all(r.join(".codex")).expect("codex");
        let user =
            json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"notify-user"}]}]}});
        fs::write(r.join(".codex/hooks.json"), user.to_string()).expect("user hooks");
        let trust = "[hooks.state.example]\ntrusted_hash = 'untouched'\n";
        fs::write(r.join(".codex/config.toml"), trust).expect("trust fixture");
        let server = Server {
            command: "/opt/it's tools/mdkg-next",
            args: &["mcp"],
        };
        install_changing(&r, &server, &["hooks:codex"], &[], true, None).expect("dry run");
        assert!(!r.join(".mdkg/hooks/codex/hooks.json").exists());
        assert_eq!(object(&r.join(".codex/hooks.json")), user);
        install_changing(&r, &server, &["hooks:codex"], &[], false, None).expect("enable");
        let canonical =
            fs::read_to_string(r.join(".mdkg/hooks/codex/hooks.json")).expect("canonical");
        assert_eq!(
            canonical,
            crate::codex_hooks::codex_fragment(server.command)
        );
        let target = r.join(".codex/hooks.json");
        let projected = object(&target);
        assert_eq!(projected["hooks"]["Stop"][0], user["hooks"]["Stop"][0]);
        assert_eq!(
            projected["hooks"]["Stop"].as_array().expect("stop").len(),
            2
        );
        assert_eq!(status(&r, Harness::Codex), None);
        assert!(
            !crate::codex_hooks::only_mdkg_hooks(&r, Path::new(server.command)),
            "user hooks cannot enable the bypass"
        );
        let before = fs::read(&target).expect("target");
        install_changing(&r, &server, &[], &[], false, None).expect("sync");
        assert_eq!(fs::read(&target).expect("target"), before);
        assert_eq!(
            fs::read_to_string(r.join(".codex/config.toml")).expect("trust"),
            trust
        );
        // Drift is reported without writing trust or silently treating it as synced.
        fs::write(&target, user.to_string()).expect("drift");
        assert!(status(&r, Harness::Codex).is_some());
        fs::write(&target, before).expect("restore fixture");
        install_changing(&r, &server, &[], &["hooks:codex"], false, None).expect("disable");
        assert_eq!(object(&target), user);
        assert!(r.join(".mdkg/hooks/codex/hooks.json").is_file());
        assert_eq!(
            fs::read_to_string(r.join(".codex/config.toml")).expect("trust"),
            trust
        );
    }
    #[test]
    fn codex_undo_restores_user_bytes_and_removes_new_canonical() {
        use crate::install::{DEFAULT_SERVER, install_changing, undo};
        use std::fs;
        let f = CodexFixture::new();
        let r = f.root();
        fs::create_dir_all(r.join(".codex")).expect("codex");
        let user = "{ \"hooks\": { \"Stop\": [{\"hooks\":[{\"type\":\"command\",\"command\":\"notify-user\"}]}] } }\n";
        fs::write(r.join(".codex/hooks.json"), user).expect("user hooks");
        let undo_at = f.0.join("undo/record.json");
        install_changing(
            &r,
            &DEFAULT_SERVER,
            &["hooks:codex"],
            &[],
            false,
            Some(&undo_at),
        )
        .expect("enable");
        undo(&r, &undo_at, false).expect("undo");
        assert_eq!(
            fs::read_to_string(r.join(".codex/hooks.json")).expect("restored"),
            user
        );
        assert!(!r.join(".mdkg/hooks/codex/hooks.json").exists());
        assert_eq!(
            object(&r.join(".mdkg/config.json")),
            json!({"projections":[]})
        );
    }
    #[test]
    fn codex_disable_recognizes_quoted_generated_commands_without_canonical() {
        use crate::install::{Server, install_changing};
        use std::fs;
        let f = CodexFixture::new();
        let r = f.root();
        let server = Server {
            command: "/opt/it's tools/mdkg-next",
            args: &["mcp"],
        };
        install_changing(&r, &server, &["hooks:codex"], &[], false, None).expect("enable");
        assert!(crate::codex_hooks::only_mdkg_hooks(
            &r,
            Path::new(server.command)
        ));
        fs::remove_file(r.join(".mdkg/hooks/codex/hooks.json")).expect("canonical gone");
        install_changing(&r, &server, &[], &["hooks:codex"], false, None).expect("disable");
        assert!(!r.join(".codex/hooks.json").exists());
    }

    #[test]
    fn mdkg_hook_commands_are_known_by_shape() {
        assert!(is_mdkg_hook("mdkg hook start --harness claude"));
        assert!(is_mdkg_hook("'/opt/my tools/mdkg-next' hook stop"));
        assert!(is_mdkg_hook("/usr/local/bin/mdkg hook edit"));
        assert!(!is_mdkg_hook("mdkg status"));
        assert!(!is_mdkg_hook("./lint.sh hook"));
        assert!(!is_mdkg_hook("echo mdkg hook"));
        // More than one simple command may hold the user's own.
        for line in [
            "mdkg hook stop --harness claude && ./notify.sh",
            "mdkg hook stop; ./notify.sh",
            "mdkg hook stop | tee log",
            "mdkg hook stop > /tmp/x",
            "mdkg hook stop $(./mine.sh)",
            "mdkg hook stop `./mine.sh`",
            "mdkg hook \"$X\"",
            "mdkg hook stop 'unclosed",
            "mdkg hook stop &",
        ] {
            assert!(!is_mdkg_hook(line), "{line}");
        }
        assert!(is_mdkg_hook("\"/opt/mdkg\" hook stop --harness codex"));
        assert!(!runs_mdkg_hook(&json!({"hooks": []})));
        assert!(!runs_mdkg_hook(&json!({"hooks": [
            {"type": "command", "command": "mdkg hook stop"},
            {"type": "command", "command": "./mine.sh"}
        ]})));
    }

    #[test]
    fn a_merge_keeps_the_users_hooks_and_replaces_only_mdkgs() {
        let root = Path::new("/nonexistent");
        let path = root.join(".claude/settings.json");
        let ours = json!({
            "Stop": [{"hooks": [{"type": "command", "command": "mdkg hook stop"}]}]
        });
        let ours = ours.as_object().expect("object").clone();
        let user = json!({
            "model": "opus",
            "hooks": {
                "Stop": [
                    {"hooks": [{"type": "command", "command": "/old/mdkg hook stop"}]},
                    {"hooks": [{"type": "command", "command": "./notify.sh"}]},
                    {"hooks": [{"type": "command", "command": "mdkg hook stop && ./notify.sh"}]}
                ],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "./guard.sh"}]}]
            }
        });
        let user = user.as_object().expect("object").clone();
        let merged = merge(root, &path, user.clone(), &ours).expect("merge");
        assert_eq!(
            Value::Object(merged.clone()),
            json!({
                "model": "opus",
                "hooks": {
                    "Stop": [
                        {"hooks": [{"type": "command", "command": "./notify.sh"}]},
                        {"hooks": [{"type": "command", "command": "mdkg hook stop && ./notify.sh"}]},
                        {"hooks": [{"type": "command", "command": "mdkg hook stop"}]}
                    ],
                    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "./guard.sh"}]}]
                }
            })
        );
        // Again: the same.
        assert_eq!(
            merge(root, &path, merged.clone(), &ours).expect("again"),
            merged
        );
        // Taken out: the user's, as they were less the old build's entry.
        let left = unmerge(root, &path, merged, &ours).expect("unmerge");
        let mut want = user;
        want["hooks"]["Stop"]
            .as_array_mut()
            .expect("list")
            .remove(0);
        assert_eq!(left, want);
        // Only mdkg's: nothing left.
        let only = merge(root, &path, Map::new(), &ours).expect("merge");
        assert!(
            unmerge(root, &path, only, &ours)
                .expect("unmerge")
                .is_empty()
        );
        // Not a fragment, or hooks that aren't lists: refused.
        assert!(fragment(root, &path, "{\"hooks\": {\"Stop\": {}}}").is_err());
        assert!(fragment(root, &path, "{\"hooks\": {}, \"model\": \"x\"}").is_err());
        let odd = json!({"hooks": {"Stop": "x"}})
            .as_object()
            .expect("o")
            .clone();
        assert!(merge(root, &path, odd, &ours).is_err());
    }
}
