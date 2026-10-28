//! The hook projections: each harness's hooks, canonical under
//! `.mdkg/hooks/<harness>/`, projected into the file the harness reads.
//! Claude Code's and Codex's are merged into a settings file the user's
//! own hooks share; OpenCode's is a plugin file of its own. Enabling one
//! never trusts it for the harness: each asks its user to review hooks.

use super::{
    Action, Op, Planned, Value, locate, plan_text, pretty, read_object, read_text, unmergeable,
};
use crate::error::RunError;
use crate::projection::Harness;
use serde_json::Map;
use std::path::Path;

/// Where a harness's hooks live: the canonical file, the file the harness
/// reads, and whether the second is shared (merged) or wholly mdkg's.
pub(super) struct Layout {
    pub canonical: &'static str,
    pub target: &'static str,
    pub merged: bool,
}

pub(super) fn layout(h: Harness) -> Layout {
    match h {
        Harness::Claude => Layout {
            canonical: ".mdkg/hooks/claude/settings.json",
            target: ".claude/settings.json",
            merged: true,
        },
        Harness::Codex => Layout {
            canonical: ".mdkg/hooks/codex/hooks.json",
            target: ".codex/hooks.json",
            merged: true,
        },
        Harness::OpenCode => Layout {
            canonical: ".mdkg/hooks/opencode/mdkg/server.ts",
            target: ".opencode/plugins/mdkg/server.ts",
            merged: false,
        },
    }
}

/// This build's canonical hooks for `h`, naming `bin`, written when the
/// project has none.
fn shipped(h: Harness, bin: &str) -> String {
    match h {
        Harness::Claude => crate::hooks::claude_canonical(bin),
        Harness::Codex => crate::codex_hooks::codex_fragment(bin),
        Harness::OpenCode => crate::hooks::opencode_plugin(bin),
    }
}

/// What syncing `h`'s hooks writes: the canonical file when the project
/// has none, then its projection.
pub(super) fn plans(root: &Path, bin: &str, h: Harness) -> Result<Vec<Planned>, RunError> {
    let l = layout(h);
    let canonical_path = root.join(l.canonical);
    let (text, at) = read_text(root, &canonical_path)?;
    let mut plan = Vec::new();
    let canonical = match text {
        Some(t) => t,
        None => {
            let t = shipped(h, bin);
            plan.push(plan_text(canonical_path.clone(), at, None, t.clone()));
            t
        }
    };
    let target = root.join(l.target);
    if l.merged {
        let ours = fragment(root, &canonical_path, &canonical)?;
        let (old, obj, at) = read_object(root, &target)?;
        let merged = merge(root, &target, obj, &ours)?;
        plan.push(plan_text(
            target,
            at,
            old.as_deref(),
            pretty(&Value::Object(merged)),
        ));
    } else {
        let (old, at) = read_text(root, &target)?;
        plan.push(plan_text(target, at, old.as_deref(), canonical));
    }
    Ok(plan)
}

/// What disabling `h`'s hooks removes: mdkg's entries from the shared
/// file, which goes once nothing else is left, or the plugin file when
/// it's the canonical one. The canonical file stays.
pub(super) fn removals(root: &Path, h: Harness) -> Result<Vec<Planned>, RunError> {
    let l = layout(h);
    let canonical_path = root.join(l.canonical);
    let canonical = read_text(root, &canonical_path)?.0;
    let target = root.join(l.target);
    if !l.merged {
        let (old, _) = read_text(root, &target)?;
        return match old {
            None => Ok(Vec::new()),
            Some(t) if Some(&t) == canonical.as_ref() => Ok(vec![Planned {
                at: locate(root, &target, false)?,
                path: target,
                action: Action::Removed,
                op: Op::Remove,
            }]),
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
        Some(t) => fragment(root, &canonical_path, t)?,
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
    let canonical_path = root.join(l.canonical);
    let Ok(canonical) = std::fs::read_to_string(&canonical_path) else {
        return Some(format!(
            "hooks:{} is enabled, but {} is missing",
            h.name(),
            l.canonical
        ));
    };
    let target = root.join(l.target);
    let current = std::fs::read_to_string(&target).ok();
    let synced = if l.merged {
        let Ok(ours) = fragment(root, &canonical_path, &canonical) else {
            return Some(format!("{} isn't a hooks fragment", l.canonical));
        };
        let obj = current
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        merge(root, &target, obj.clone(), &ours).is_ok_and(|m| m == obj)
    } else {
        current.as_deref() == Some(canonical.as_str())
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
