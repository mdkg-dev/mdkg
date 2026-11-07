use super::*;
use hook::event::EventName;
use hook::{Event, Outcome};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mdkg-adapter-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&root).expect("fixture");
        Self(root)
    }
    fn input(&self, harness: Harness, role: Role) -> TurnInput<'_> {
        TurnInput {
            agent: "adapter-test",
            mdkg: Path::new("/fixture/bin/mdkg"),
            home: Some(&self.0),
            xdg_config: Some(&self.0),
            common_dir: None,
            trusted_worktree: None,
            graph_root: None,
            main_ref: Some("main"),
            harness,
            role,
            cwd: &self.0,
            model: None,
            effort: None,
            sandbox: None,
            approvals: None,
            resume: None,
            new_session: "0199a0a0-0000-7000-8000-000000000001",
            allow: &[],
            output_file: Path::new("/fixture/reply"),
            prompt: "fixture prompt",
            api_key: None,
        }
    }
    fn program(&self) -> PathBuf {
        let path = self.0.join("fake-harness");
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "case \"$1 $2\" in\n",
                "  'auth status') echo '{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}' ;;\n",
                "  'login status') echo 'Logged in using ChatGPT' ;;\n",
                "  *) exit 1 ;;\n",
                "esac\n",
            ),
        )
        .expect("fake program");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("mode");
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn every_registered_harness_implements_the_complete_trait() {
    // Registering a fourth adapter cannot omit a method or a runtime test.
    let ids: Vec<_> = adapters().map(HarnessAdapter::id).collect();
    assert_eq!(ids, ["claude", "codex", "opencode"]);
    for h in Harness::ALL {
        let fixture = Fixture::new();
        let adapter: &dyn HarnessAdapter = h.adapter();
        assert_eq!(Harness::parse(adapter.id()), Some(h));
        assert!(!adapter.display_name().is_empty());
        assert_eq!(
            adapter.program_override(),
            format!("MDKG_RUN_{}", h.as_str().to_uppercase())
        );
        assert_eq!(adapter.version_probe(), ["--version"]);
        assert!(adapter.capabilities().contains(&"resume"));
        assert!(
            !adapter
                .capabilities()
                .iter()
                .any(|c| c.starts_with("output."))
        );
        assert!(adapter.withheld(OsStr::new("OPENAI_API_KEY")));
        assert!(adapter.withheld(OsStr::new("CLAUDE_CODE_SESSION_ID")));
        assert!(!adapter.withheld(OsStr::new("PATH")));
        assert!(adapter.validate_overrides(None, None).is_ok());
        if let Some(efforts) = adapter.efforts() {
            for effort in efforts {
                assert!(adapter.validate_effort(effort).is_ok());
            }
        }
        assert!(adapter.validate_effort("not-an-effort").is_err());

        for role in [Role::Worker, Role::Reviewer] {
            for resume in [None, Some("session-fixture")] {
                let mut input = fixture.input(h, role);
                input.resume = resume;
                let command = adapter.launch(&input, Some(input.mdkg));
                assert_eq!(command.program, h.program());
                assert_eq!(command.cwd, fixture.0);
                assert_eq!(command.env, adapter.environment(&input));
                assert!(
                    command
                        .env
                        .contains(&("MDKG_ACTOR".into(), "adapter-test".into()))
                );
                assert_eq!(command.prompt_on_stdin, h != Harness::OpenCode);
                if let Some(session) = resume {
                    assert!(command.args.iter().any(|a| a == session));
                }
                if role == Role::Reviewer {
                    // No native probe or model is started for a reviewer.
                    adapter.preflight(&command).expect("reviewer preflight");
                }
            }
        }

        let mcp = adapter.mcp_projection();
        assert_eq!(mcp.name, format!("mcp:{}", adapter.id()));
        assert!(crate::projection::NAMES.contains(&mcp.name));
        assert!(!mcp.present(&fixture.0));
        assert_eq!(mcp.command(&fixture.0), None);
        let plan = adapter
            .mcp_project(&fixture.0, &crate::install::DEFAULT_SERVER)
            .expect("MCP plan");
        assert!(format!("{plan:?}").contains(mcp.path.split('/').next_back().expect("file")));
        assert!(
            !fixture.0.join(mcp.path).exists(),
            "planning never publishes"
        );
        let file = fixture.0.join(mcp.path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("config folder");
        let text = match h {
            Harness::Claude => "{\"mcpServers\":{\"mdkg\":{\"command\":\"fixture-mdkg\"}}}",
            Harness::Codex => "[mcp_servers.mdkg]\ncommand = 'fixture-mdkg'\n",
            Harness::OpenCode => "{\"mcp\":{\"mdkg\":{\"command\":[\"fixture-mdkg\"]}}}",
        };
        std::fs::write(file, text).expect("MCP fixture");
        assert!(mcp.present(&fixture.0));
        assert_eq!(mcp.command(&fixture.0).as_deref(), Some("fixture-mdkg"));
        let hooks = adapter.hook_layout();
        assert_eq!(hooks.name, format!("hooks:{}", adapter.id()));
        assert!(crate::projection::NAMES.contains(&hooks.name));
        assert!(hooks.canonical.starts_with(".mdkg/hooks/"));
        assert!(!hooks.target.is_empty());
        assert!(!adapter.hook_project("fixture-mdkg").is_empty());
        assert!(adapter.hook_supports(EventName::ToolBefore));
        assert!(!adapter.hook_supports(EventName::GitPrePush));
        assert_eq!(
            adapter.hook_supports(EventName::SessionIdle),
            h != Harness::Codex
        );
        assert_eq!(adapter.hook_native_event("unknown-event"), None);
        assert_eq!(
            adapter.hook_native_event("idle").is_some(),
            h != Harness::Codex
        );

        let native = if h == Harness::OpenCode {
            "tool.execute.before"
        } else {
            "PreToolUse"
        };
        assert_eq!(
            adapter.hook_native_event(native),
            Some((Event::Edit, EventName::ToolBefore))
        );
        let wire = match h {
            Harness::Claude => json!({"session_id":"session-fixture", "cwd":fixture.0,
                "hook_event_name":"PreToolUse", "tool_name":"Write", "tool_input":{"file_path":"file.rs"}}),
            Harness::Codex => json!({"session_id":"session-fixture", "cwd":fixture.0,
                "hook_event_name":"PreToolUse", "tool_name":"apply_patch",
                "tool_input":{"command":"*** Begin Patch\n*** Add File: file.rs\n+fixture\n*** End Patch"}}),
            Harness::OpenCode => json!({"sessionID":"session-fixture", "cwd":fixture.0,
                "tool":"write", "args":{"path":"file.rs"}}),
        };
        let input = adapter.hook_parse(Event::Edit, &wire).expect("native hook");
        assert_eq!(
            input.paths,
            ["file.rs"],
            "{} native edit fixture",
            adapter.id()
        );
        assert_eq!(input.session.as_deref(), Some("session-fixture"));
        let rendered = adapter.hook_render(Event::Edit, &Outcome::Deny("claim first".into()));
        assert!(rendered.stdout.contains("claim first") || rendered.stderr.contains("claim first"));
        let binding = adapter.hook_bound_actor(&fixture.0, &input);
        if h == Harness::Codex {
            assert!(binding.is_err(), "non-repository binding fails closed");
        } else {
            assert_eq!(binding, Ok(None));
        }

        let program = fixture.program();
        let env = crate::env::Environment::allowlist(h);
        let login = adapter.login(&program, &fixture.0, &env);
        let keys = KeySource {
            program: program.clone(),
            limit: std::time::Duration::from_secs(2),
        };
        let status = adapter.auth_status(
            &program,
            &fixture.0,
            &crate::policy::Auth::default(),
            &keys,
            &env,
        );
        assert_eq!(
            adapter.login_command(),
            format!(
                "{}{}",
                adapter.id(),
                if h == Harness::Codex {
                    " login"
                } else {
                    " auth login"
                }
            )
        );
        if h == Harness::OpenCode {
            assert_eq!(login, Login::Own);
            assert_eq!(adapter.keychain_service(), None);
            assert_eq!(status.source, "own_config");
        } else {
            assert!(matches!(
                login,
                Login::In {
                    subscription: true,
                    ..
                }
            ));
            assert!(adapter.keychain_service().is_some());
            assert_eq!(status.source, "subscription");
        }
        assert!(!status.key_in_keychain);
        assert_eq!(crate::auth::login(h, &program, &fixture.0, &env), login);
        assert_eq!(crate::auth::login_command(h), adapter.login_command());
        assert_eq!(crate::auth::keychain_service(h), adapter.keychain_service());
    }
}

#[test]
fn each_adapter_knows_only_its_own_usage_limit_line() {
    let lines = [
        (
            Harness::Claude,
            "{\"type\":\"result\",\"is_error\":true,\"result\":\"Claude AI usage limit reached|1\"}",
        ),
        (
            Harness::Codex,
            "ERROR: You've hit your usage limit. Try later.",
        ),
        // OpenCode's provider form: Codex's own JSON events read a nested
        // `code` too, so its `data.message` form tells the two apart.
        (
            Harness::OpenCode,
            "{\"type\":\"error\",\"error\":{\"data\":{\"message\":\"usage_limit_reached\"}}}",
        ),
    ];
    for adapter in adapters() {
        let h = Harness::parse(adapter.id()).expect("registered id");
        for (owner, line) in lines {
            assert_eq!(
                adapter.usage_limit(line),
                owner == h,
                "{} reading {}'s line",
                adapter.id(),
                owner.as_str()
            );
            assert_eq!(
                crate::auth::usage_limit_error(h, line),
                adapter.usage_limit(line)
            );
            assert_eq!(
                adapter.usage_limit_in(&[line.to_string()]),
                owner == h,
                "{} reading {}'s stream",
                adapter.id(),
                owner.as_str()
            );
        }
        assert!(!adapter.usage_limit("usage limit reached, quoted in a reply"));
        assert!(!adapter.usage_limit(""));
    }
}

#[test]
fn all_three_adapters_normalize_session_reply_and_usage() {
    for (h, stdout, stderr, output) in [
        (
            Harness::Claude,
            "{\"type\":\"result\",\"session_id\":\"session-fixture\",\"result\":\"fixture reply\",\"usage\":{\"input_tokens\":3}}",
            "",
            None,
        ),
        (
            Harness::Codex,
            "",
            "--------\nsandbox: read-only\napproval: never\nsession id: session-fixture\n--------\ntokens used\n3\n",
            Some("fixture reply"),
        ),
        (
            Harness::OpenCode,
            "{\"type\":\"text\",\"sessionID\":\"session-fixture\",\"part\":{\"text\":\"fixture reply\",\"tokens\":{\"input\":3}}}",
            "",
            None,
        ),
    ] {
        let parsed = h
            .adapter()
            .parse_output(
                Role::Reviewer,
                stdout,
                stderr,
                output,
                crate::profile::codex_settings(Role::Reviewer, None, None),
            )
            .expect("normalized output");
        assert_eq!(parsed.session.as_deref(), Some("session-fixture"));
        assert_eq!(parsed.reply, "fixture reply");
        let usage = parsed.usage.expect("usage");
        assert_eq!(
            if h == Harness::Codex {
                usage.total_tokens
            } else {
                usage.input_tokens
            },
            Some(3)
        );
    }
}

#[test]
fn identity_serialization_and_schema_keep_the_shipped_values() {
    for h in Harness::ALL {
        let value = json!(h.as_str());
        assert_eq!(serde_json::to_value(h).expect("serialize"), value);
        assert_eq!(
            serde_json::from_value::<Harness>(value).expect("deserialize"),
            h
        );
    }
    assert!(serde_json::from_value::<Harness>(json!("unknown")).is_err());
    let schema = serde_json::to_value(schemars::schema_for!(Harness)).expect("schema");
    assert_eq!(schema["title"], "Harness");
    assert_eq!(schema["enum"], json!(["claude", "codex", "opencode"]));
}

/// The runner's side of a turn (edd-01a1a454, step 1), as each built-in
/// adapter answers it: the vendor points turn.rs used to match on by name.
#[test]
fn each_adapter_answers_the_runners_side_of_a_turn() {
    let fixture = Fixture::new();
    let opts = crate::turn::TurnOptions::default();
    let turns = fixture.0.join("turns");
    let settings = crate::profile::codex_settings(Role::Worker, None, None);
    for h in Harness::ALL {
        let a = h.adapter();
        let claude = h == Harness::Claude;
        let codex = h == Harness::Codex;
        let opencode = h == Harness::OpenCode;
        assert_eq!(a.prompt_max(), opencode.then_some(96 * 1024), "{h:?}");
        assert_eq!(a.chooses_session(), claude, "{h:?}");
        assert_eq!(a.reply_on_stdout(), codex, "{h:?}");
        assert_eq!(a.structured_output(), !codex, "{h:?}");
        assert!(a.early_check("", Role::Worker, settings).is_none(), "{h:?}");
        // A Zen model is refused only by OpenCode, and only unless allowed.
        let zen = a.check_turn(Some("opencode/some-model"), &opts);
        assert_eq!(zen.is_err(), opencode, "{h:?}");
        let allowed = crate::turn::TurnOptions {
            allow_zen: true,
            ..crate::turn::TurnOptions::default()
        };
        assert!(a.check_turn(Some("opencode/some-model"), &allowed).is_ok());
        assert!(a.check_turn(None, &opts).is_ok());
        // With no v0.1 home and no Codex session file, only Codex refuses
        // to resume.
        let home = fixture.0.join("home");
        let codex_home = crate::turn::TurnOptions {
            user_codex_home: Some(fixture.0.join("codex")),
            ..crate::turn::TurnOptions::default()
        };
        assert_eq!(
            a.resumable("s-1", false, &turns, &codex_home, Some(&home)),
            !codex,
            "{h:?}"
        );
    }
    // A Codex session its home holds resumes; an OpenCode one left in the
    // v0.1 private home doesn't unless the user's own.
    let codex_home = fixture.0.join("codex");
    let day = codex_home.join("sessions/2026/11/04");
    std::fs::create_dir_all(&day).expect("sessions");
    std::fs::write(day.join("rollout-x-s-1.jsonl"), "").expect("session");
    let with_home = crate::turn::TurnOptions {
        user_codex_home: Some(codex_home),
        ..crate::turn::TurnOptions::default()
    };
    assert!(
        Harness::Codex
            .adapter()
            .resumable("s-1", false, &turns, &with_home, None)
    );
    assert!(
        !Harness::Codex
            .adapter()
            .resumable("s-2", false, &turns, &with_home, None)
    );
    std::fs::create_dir_all(turns.join("home")).expect("v0.1 home");
    let oc = Harness::OpenCode.adapter();
    assert!(!oc.resumable("s-1", false, &turns, &opts, None));
    assert!(oc.resumable("s-1", true, &turns, &opts, None));
}
