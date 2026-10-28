//! Codex 0.164 command-hook definitions and the launcher's conservative
//! project-file predicate. No trust hashes are read or written here.
use std::{fs::File, io::Read, path::Path};

use serde::{Deserialize, Deserializer};
use serde_json::json;

/// The canonical fragment for install's opt-in `hooks:codex` projection.
/// `bin` is one executable name/path, never a shell command with arguments.
/// Its spelling stays stable across upgrades; no versioned path is resolved.
pub fn codex_fragment(bin: &str) -> String {
    let command = |event| json!({"type":"command", "command": hook_command(bin, event)});
    let fragment = json!({"hooks": {
        "SessionStart": [{"hooks": [command("start")]}],
        "UserPromptSubmit": [{"hooks": [command("start")]}],
        "Stop": [{"hooks": [command("stop")]}],
        "PreToolUse": [{"matcher": "apply_patch", "hooks": [command("edit")]}]
    }});
    format!("{fragment:#}\n")
}

fn shell_word(bin: &str) -> String {
    if !bin.is_empty()
        && bin
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c))
    {
        bin.into()
    } else {
        // Adjacent quote segments avoid an unquoted backslash: the install
        // merger can also recognize this command when the canonical is gone.
        format!("'{}'", bin.replace('\'', "'\"'\"'"))
    }
}
fn hook_command(bin: &str, event: &str) -> String {
    format!("{} hook {event} --harness codex", shell_word(bin))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    hooks: Events,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Events {
    #[serde(rename = "SessionStart", default)]
    session_start: Vec<Entry>,
    #[serde(rename = "UserPromptSubmit", default)]
    user_prompt_submit: Vec<Entry>,
    #[serde(rename = "Stop", default)]
    stop: Vec<Entry>,
    #[serde(rename = "PreToolUse", default)]
    pre_tool_use: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    #[serde(default, deserialize_with = "matcher")]
    matcher: Option<String>,
    hooks: Vec<Hook>,
}
fn matcher<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    String::deserialize(d).map(Some)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hook {
    #[serde(rename = "type")]
    kind: String,
    command: String,
}

fn only_ours(bytes: &[u8], bin: &str) -> bool {
    if bin.is_empty() || bin.chars().any(char::is_control) {
        return false;
    }
    // Typed structs reject duplicate keys, not just unknown fields. Parsing
    // into Value first would silently discard duplicate execution fields.
    let Ok(doc) = serde_json::from_slice::<Document>(bytes) else {
        return false;
    };
    let events = [
        (&doc.hooks.session_start, "start", None),
        (&doc.hooks.user_prompt_submit, "start", None),
        (&doc.hooks.stop, "stop", None),
        (&doc.hooks.pre_tool_use, "edit", Some("apply_patch")),
    ];
    events.iter().any(|(entries, _, _)| !entries.is_empty())
        && events.iter().all(|(entries, event, matcher)| {
            entries.len() <= 1
                && entries.iter().all(|entry| {
                    entry.matcher.as_deref() == *matcher
                        && entry.hooks.len() == 1
                        && entry.hooks[0].kind == "command"
                        && entry.hooks[0].command == hook_command(bin, event)
                })
        })
}

/// True only for a nonempty project `.codex/hooks.json` containing known mdkg
/// entries, with commands exactly generated for `mdkg`. Subsets are allowed;
/// unknown events/options, extra commands, changed matchers, duplicate keys,
/// missing/unreadable/oversized hook files and links refuse. The sibling
/// config.toml must be absent or bounded, parseable TOML with no [hooks] key.
///
/// This is a project-file predicate, NEVER sufficient authority for Codex's
/// invocation-wide hook-trust bypass. It does not inventory user/system,
/// session, profile, plugin or managed sources, set trust, resolve executable
/// aliases, or protect against edits after the check.
pub fn only_mdkg_hooks(root: &Path, mdkg: &Path) -> bool {
    let Some(bin) = mdkg.to_str() else {
        return false;
    };
    const CAP: u64 = 64 * 1024;
    use rustix::fs::{Mode, OFlags, open, openat};
    // Open the child relative to the already checked directory descriptor:
    // swapping `.codex` to a link cannot redirect the read after its check.
    let Ok(dir) = open(
        root.join(".codex"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) else {
        return false;
    };
    let read = |name: &str| -> Result<Option<Vec<u8>>, ()> {
        let fd = match openat(
            &dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(()),
        };
        let file = File::from(fd);
        if !file.metadata().is_ok_and(|m| m.is_file() && m.len() <= CAP) {
            return Err(());
        }
        let mut bytes = Vec::new();
        file.take(CAP + 1).read_to_end(&mut bytes).map_err(|_| ())?;
        if bytes.len() as u64 > CAP {
            return Err(());
        }
        Ok(Some(bytes))
    };
    let Ok(Some(bytes)) = read("hooks.json") else {
        return false;
    };
    if !only_ours(&bytes, bin) {
        return false;
    }
    match read("config.toml") {
        Ok(None) => true,
        Ok(Some(bytes)) => std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
            .is_some_and(|config| !config.contains_key("hooks")),
        Err(()) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{fs, path::PathBuf};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("mdkg-codex-hooks-{}", uuid::Uuid::now_v7()));
            fs::create_dir_all(p.join(".codex")).expect("fixture");
            Self(p)
        }
        fn write(&self, text: &str) {
            fs::write(self.0.join(".codex/hooks.json"), text).expect("hooks");
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn valid() -> Value {
        serde_json::from_str(&codex_fragment("mdkg-next")).expect("fragment")
    }
    fn accepts(v: &Value) -> bool {
        only_ours(&serde_json::to_vec(v).expect("JSON"), "mdkg-next")
    }

    #[test]
    fn four_events_have_the_adapter_commands_and_native_edit_matcher() {
        let v = valid();
        assert!(accepts(&v));
        assert_eq!(v["hooks"].as_object().expect("events").len(), 4);
        for (event, cmd) in [
            ("SessionStart", "start"),
            ("UserPromptSubmit", "start"),
            ("Stop", "stop"),
            ("PreToolUse", "edit"),
        ] {
            assert_eq!(
                v["hooks"][event][0]["hooks"][0]["command"],
                format!("mdkg-next hook {cmd} --harness codex")
            );
        }
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "apply_patch");
        let stop = json!({"hooks":{"Stop":v["hooks"]["Stop"]}});
        assert!(accepts(&stop));
        for empty in [json!({}), json!({"hooks":{}}), json!({"hooks":{"Stop":[]}})] {
            assert!(!accepts(&empty));
        }
    }
    #[test]
    fn command_spelling_is_exact_and_shell_syntax_cannot_extend_it() {
        for command in [
            "mdkg-evil hook stop --harness codex",
            "mdkg-next hook stop --harness claude",
            "mdkg-next hook stop --harness codex; echo bad",
            "mdkg-next hook stop --harness codex && evil",
            "mdkg-next hook stop --harness codex > out",
            "mdkg-next hook stop --harness codex $(evil)",
            "mdkg-next hook stop --harness codex `evil`",
            "mdkg-next hook stop --harness codex\nevil",
            "mdkg-next hook stop --harness codex --as someone",
            "env mdkg-next hook stop --harness codex",
            "./mdkg-next hook stop --harness codex",
            "mdkg-next hook edit --harness codex",
        ] {
            let mut v = valid();
            v["hooks"]["Stop"][0]["hooks"][0]["command"] = json!(command);
            assert!(!accepts(&v), "{command}");
        }
    }
    #[test]
    fn unknown_options_events_matchers_and_extra_hooks_refuse() {
        for field in ["async", "timeout", "once", "statusMessage", "prompt"] {
            let mut v = valid();
            v["hooks"]["Stop"][0]["hooks"][0][field] = json!(true);
            assert!(!accepts(&v));
        }
        let mut v = valid();
        v["hooks"]["Stop"][0]["matcher"] = json!("*");
        assert!(!accepts(&v));
        v = valid();
        v["hooks"]["PreToolUse"][0]["matcher"] = json!(".*");
        assert!(!accepts(&v));
        v = valid();
        v["hooks"]["PreToolUse"][0]["matcher"] = Value::Null;
        assert!(!accepts(&v));
        v = valid();
        v["hooks"]["Stop"][0]["hooks"][0]["type"] = json!("prompt");
        assert!(!accepts(&v));
        v = valid();
        v["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .expect("hooks")
            .push(json!({"type":"command","command":"other"}));
        assert!(!accepts(&v));
        v = valid();
        v["hooks"]["Other"] = json!([]);
        assert!(!accepts(&v));
        v = valid();
        v["extra"] = json!({});
        assert!(!accepts(&v));
        v = valid();
        let entry = v["hooks"]["Stop"][0].clone();
        v["hooks"]["Stop"]
            .as_array_mut()
            .expect("entries")
            .push(entry);
        assert!(!accepts(&v));
    }
    #[test]
    fn duplicate_keys_are_not_overwritten_into_trusted_shapes() {
        for text in [
            r#"{"hooks":{},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"mdkg-next hook stop --harness codex"}]}]}}"#,
            r#"{"hooks":{"Stop":[],"Stop":[{"hooks":[{"type":"command","command":"mdkg-next hook stop --harness codex"}]}]}}"#,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"evil","command":"mdkg-next hook stop --harness codex"}]}]}}"#,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"prompt","type":"command","command":"mdkg-next hook stop --harness codex"}]}]}}"#,
        ] {
            assert!(!only_ours(text.as_bytes(), "mdkg-next"));
        }
    }
    #[test]
    fn paths_are_quoted_as_one_literal_word_and_not_resolved_to_versions() {
        for bin in [
            "mdkg-next",
            "/usr/local/bin/mdkg-next",
            "/opt/my tools/mdkg-next",
            "/opt/it's tools/mdkg-next",
            "/opt/$(bad)/mdkg-next",
        ] {
            assert!(only_ours(codex_fragment(bin).as_bytes(), bin));
        }
        assert_eq!(
            shell_word("/opt/it's tools/mdkg-next"),
            "'/opt/it'\"'\"'s tools/mdkg-next'"
        );
        assert!(!only_ours(codex_fragment("mdkg").as_bytes(), "mdkg-next"));
        assert!(!only_ours(codex_fragment("").as_bytes(), ""));
        assert!(!only_ours(
            codex_fragment("bad\nname").as_bytes(),
            "bad\nname"
        ));
    }
    #[test]
    fn generated_command_executes_one_quoted_binary_with_literal_arguments() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let bin = f.0.join("It's $(touch BAD) tools/mdkg-next");
        fs::create_dir_all(bin.parent().expect("parent")).expect("directory");
        fs::write(&bin, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\n").expect("stub");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).expect("executable");
        let capture = f.0.join("args");
        let command = hook_command(bin.to_str().expect("utf8"), "edit");
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .current_dir(&f.0)
            .env("CAPTURE", &capture)
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .status()
            .expect("execute generated command");
        assert!(status.success());
        assert_eq!(
            fs::read_to_string(capture).expect("args"),
            "hook\nedit\n--harness\ncodex\n"
        );
        assert!(!f.0.join("BAD").exists());
    }

    #[test]
    fn filesystem_predicate_refuses_missing_malformed_oversized_and_linked_files() {
        let f = Fixture::new();
        let bin = Path::new("mdkg-next");
        assert!(!only_mdkg_hooks(&f.0, bin));
        f.write(&codex_fragment("mdkg-next"));
        assert!(only_mdkg_hooks(&f.0, bin));
        for text in ["broken".into(), " ".repeat(65537)] {
            f.write(&text);
            assert!(!only_mdkg_hooks(&f.0, bin));
        }
        let target = f.0.join("real.json");
        fs::write(&target, codex_fragment("mdkg-next")).expect("target");
        fs::remove_file(f.0.join(".codex/hooks.json")).expect("remove");
        std::os::unix::fs::symlink(&target, f.0.join(".codex/hooks.json")).expect("link");
        assert!(!only_mdkg_hooks(&f.0, bin));
        fs::remove_file(f.0.join(".codex/hooks.json")).expect("remove link");
        fs::rename(f.0.join(".codex"), f.0.join("real-dir")).expect("move");
        fs::write(f.0.join("real-dir/hooks.json"), codex_fragment("mdkg-next")).expect("real");
        std::os::unix::fs::symlink(f.0.join("real-dir"), f.0.join(".codex")).expect("dir link");
        assert!(!only_mdkg_hooks(&f.0, bin));
    }
    #[test]
    fn fifo_and_directory_never_block_a_trust_check() {
        let f = Fixture::new();
        let p = f.0.join(".codex/hooks.json");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&p)
                .status()
                .expect("fifo")
                .success()
        );
        let at = std::time::Instant::now();
        assert!(!only_mdkg_hooks(&f.0, Path::new("mdkg-next")));
        assert!(at.elapsed() < std::time::Duration::from_secs(1));
        fs::remove_file(&p).expect("remove fifo");
        fs::create_dir(&p).expect("directory");
        assert!(!only_mdkg_hooks(&f.0, Path::new("mdkg-next")));
    }

    #[test]
    fn sibling_toml_hooks_never_qualify_project_files() {
        let f = Fixture::new();
        let bin = Path::new("mdkg-next");
        f.write(&codex_fragment("mdkg-next"));
        assert!(only_mdkg_hooks(&f.0, bin));
        for text in [
            "[hooks]\n",
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype='command'\ncommand='foreign-hook'\n",
            "['hooks']\nStop=[]\n",
            "hooks.Stop = []\n",
            "hooks = { Stop = [] }\n",
            "hooks = {}\nhooks = {}\n",
        ] {
            fs::write(f.0.join(".codex/config.toml"), text).expect("config");
            assert!(!only_mdkg_hooks(&f.0, bin), "{text}");
        }
    }

    #[test]
    fn sibling_toml_changes_are_checked_without_hooks_json_changes() {
        let f = Fixture::new();
        f.write(&codex_fragment("mdkg-next"));
        let hooks = fs::read(f.0.join(".codex/hooks.json")).expect("hooks");
        fs::write(f.0.join(".codex/config.toml"), "model='offline'\n").expect("config");
        assert!(only_mdkg_hooks(&f.0, Path::new("mdkg-next")));
        fs::write(
            f.0.join(".codex/config.toml"),
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype='command'\ncommand='foreign-hook'\n",
        )
        .expect("changed config");
        assert!(!only_mdkg_hooks(&f.0, Path::new("mdkg-next")));
        assert_eq!(
            fs::read(f.0.join(".codex/hooks.json")).expect("hooks"),
            hooks
        );
    }

    #[test]
    fn sibling_toml_unreadable_unparseable_or_special_files_fail_closed() {
        let f = Fixture::new();
        f.write(&codex_fragment("mdkg-next"));
        let config = f.0.join(".codex/config.toml");
        for bytes in [b"broken = [".to_vec(), vec![0xff], vec![b' '; 65537]] {
            fs::write(&config, bytes).expect("config");
            assert!(!only_mdkg_hooks(&f.0, Path::new("mdkg-next")));
        }
        fs::remove_file(&config).expect("remove");
        std::os::unix::fs::symlink(f.0.join("absent"), &config).expect("dangling link");
        assert!(!only_mdkg_hooks(&f.0, Path::new("mdkg-next")));
        fs::remove_file(&config).expect("unlink");
        fs::create_dir(&config).expect("directory");
        assert!(!only_mdkg_hooks(&f.0, Path::new("mdkg-next")));
        fs::remove_dir(&config).expect("remove directory");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&config)
                .status()
                .expect("fifo")
                .success()
        );
        let at = std::time::Instant::now();
        assert!(!only_mdkg_hooks(&f.0, Path::new("mdkg-next")));
        assert!(at.elapsed() < std::time::Duration::from_secs(1));
    }
}
