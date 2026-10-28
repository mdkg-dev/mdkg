use super::*;
use serde_json::json;

#[cfg(feature = "settings-fs")]
struct Fixture(PathBuf);
#[cfg(feature = "settings-fs")]
impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "mdkg-layers-{name}-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("fixture");
        Self(std::fs::canonicalize(root).expect("canonical"))
    }
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, bytes).expect("write");
        path
    }
    fn repository(&self, gitdir: &str) {
        for name in ["objects", "refs"] {
            std::fs::create_dir_all(self.0.join(gitdir).join(name)).expect("metadata");
        }
        self.write(&format!("{gitdir}/HEAD"), "ref: refs/heads/main\n");
        self.write(&format!("{gitdir}/config"), "[core]\n bare = false\n");
    }
}
#[cfg(feature = "settings-fs")]
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("clean fixture");
    }
}

fn input(layer: Layer, value: Value) -> Input {
    Input::new(Origin::value(layer), value).expect("object input")
}
fn empty_layers() -> Layers {
    Layers {
        user: None,
        project: None,
        local: None,
    }
}

#[test]
fn five_layers_merge_fields_and_keep_each_origin() {
    let defaults = input(
        Layer::BuiltIn,
        json!({"a":{"built":1,"user":1,"project":1,"local":1,"flag":1}}),
    );
    let layers = Layers {
        user: Some(input(
            Layer::User,
            json!({"a":{"user":2,"project":2,"local":2,"flag":2}}),
        )),
        project: Some(input(
            Layer::Project,
            json!({"a":{"project":3,"local":3,"flag":3}}),
        )),
        local: Some(input(Layer::Local, json!({"a":{"local":4,"flag":4}}))),
    };
    let out = layers
        .resolve(&defaults, &[], Some(&json!({"a":{"flag":5}})))
        .expect("resolve");
    assert_eq!(
        out.values,
        json!({"a":{"built":1,"user":2,"project":3,"local":4,"flag":5}})
    );
    for (key, layer) in [
        ("built", Layer::BuiltIn),
        ("user", Layer::User),
        ("project", Layer::Project),
        ("local", Layer::Local),
        ("flag", Layer::Flags),
    ] {
        assert_eq!(out.origins[&format!("/a/{key}")].layer, layer);
    }
    assert_eq!(out.origins.len(), 5);
}

#[test]
fn definition_sources_are_project_inputs_below_local_agent_overrides() {
    let definition_path = PathBuf::from(".mdkg/agents/worker.json");
    let defaults = input(
        Layer::BuiltIn,
        json!({"agents":{"defaults":{"timeout":null}}}),
    );
    let layers = Layers {
        user: Some(input(
            Layer::User,
            json!({"agents":{"worker":{"model":"user","effort":"high"}}}),
        )),
        project: Some(input(
            Layer::Project,
            json!({"agents":{"worker":{"model":"inline","harness":"claude"}}}),
        )),
        local: Some(input(
            Layer::Local,
            json!({"agents":{"worker":{"model":"local"}}}),
        )),
    };
    let definition = Input::new(Origin::file(Layer::Project,&definition_path),
        json!({"agents":{"worker":{"model":"definition","role":"worker","policy":{"timeout":900}}}})).expect("definition");
    let out = layers
        .resolve(&defaults, &[definition], None)
        .expect("resolve");
    let agent = &out.values["agents"]["worker"];
    assert_eq!(
        agent,
        &json!({"model":"local","effort":"high","harness":"claude","role":"worker","policy":{"timeout":900}})
    );
    assert_eq!(out.origins["/agents/worker/model"].layer, Layer::Local);
    assert_eq!(out.origins["/agents/worker/effort"].layer, Layer::User);
    assert_eq!(
        out.origins["/agents/worker/role"].path.as_ref(),
        Some(&definition_path)
    );
    assert_eq!(
        out.origins["/agents/worker/policy/timeout"].path.as_ref(),
        Some(&definition_path)
    );
}

#[test]
fn replacing_containers_removes_stale_origins_but_not_adjacent_keys() {
    let defaults = input(
        Layer::BuiltIn,
        json!({"a":{"x":1,"y":2},"ab":{"keep":true},"list":[1,2,3],"empty":{}}),
    );
    let mut out = Effective::new(&defaults).expect("effective");
    out.apply(&input(
        Layer::Project,
        json!({"a":null,"list":[{"z":false}],"empty":{"new":1}}),
    ))
    .expect("replace");
    assert!(!out.origins.contains_key("/a/x") && !out.origins.contains_key("/a/y"));
    assert_eq!(out.origins["/a"].layer, Layer::Project);
    assert_eq!(out.origins["/ab/keep"].layer, Layer::BuiltIn);
    assert_eq!(out.origins["/list/0/z"].layer, Layer::Project);
    assert!(!out.origins.contains_key("/list/1") && !out.origins.contains_key("/empty"));
    out.apply(&input(Layer::Local, json!({"a":{},"list":[],"empty":{}})))
        .expect("clear list");
    assert_eq!(out.values["a"], json!({}));
    assert_eq!(
        out.values["empty"],
        json!({"new":1}),
        "empty objects don't clear keys"
    );
    assert_eq!(out.origins["/a"].layer, Layer::Local);
    assert_eq!(out.origins["/list"].layer, Layer::Local);
    assert!(!out.origins.contains_key("/list/0/z"));
}

#[test]
fn origins_escape_json_pointer_keys_and_cover_array_leaves() {
    let mut out = Effective::new(&input(Layer::BuiltIn, json!({}))).expect("effective");
    out.apply(&input(
        Layer::Project,
        json!({"a/b~c":[true,{},null,"λ"],"":[]}),
    ))
    .expect("apply");
    let expected = ["/a~1b~0c/0", "/a~1b~0c/1", "/a~1b~0c/2", "/a~1b~0c/3", "/"];
    assert_eq!(out.origins.len(), expected.len());
    for path in expected {
        assert!(out.values.pointer(path).is_some(), "{path}");
        assert_eq!(out.origins[path].layer, Layer::Project);
    }
    let encoded = serde_json::to_value(&out).expect("serialize origins");
    assert_eq!(encoded["origins"]["/"]["layer"], "project");
}

#[test]
fn invalid_json_duplicates_and_nonobjects_never_pick_a_winner() {
    for text in [
        "",
        "null",
        "[]",
        "1",
        "{} {}",
        "{\"a\":1,\"a\":2}",
        "{\"a\":{\"b\":true,\"b\":false}}",
        "{\"a\":[{\"b\":0,\"b\":1}]}",
        "{\"untrusted\": INSTRUCTION_SENTINEL}",
    ] {
        let error = Input::parse(Origin::value(Layer::User), text.as_bytes()).expect_err("refuse");
        assert_eq!(error.kind, ErrorKind::Invalid);
        assert!(!error.to_string().contains("INSTRUCTION_SENTINEL"));
    }
    let exact = Input::parse(
        Origin::value(Layer::Project),
        br#"{"s":"text","t":true,"f":false,"n":null,"i":-3,"u":18446744073709551615,"d":1.5}"#,
    )
    .expect("scalars");
    assert_eq!(exact.value["u"].as_u64(), Some(u64::MAX));
    assert_eq!(exact.value["i"].as_i64(), Some(-3));
    assert_eq!(exact.value["d"].as_f64(), Some(1.5));
}

#[test]
fn invalid_mutated_inputs_leave_the_effective_config_unchanged() {
    let mut out = Effective::new(&input(Layer::BuiltIn, json!({"keep":1}))).expect("effective");
    let before = out.clone();
    let mut bad = input(Layer::Local, json!({}));
    bad.value = json!([1, 2]);
    assert!(out.apply(&bad).is_err());
    assert_eq!(out, before);
    let mut deep = json!({});
    for _ in 0..MAX_DEPTH + 1 {
        deep = json!({"child":deep});
    }
    bad.value = json!({"first":"must not apply","last":deep});
    assert!(out.apply(&bad).is_err());
    assert_eq!(out, before);
}

#[cfg(feature = "settings-fs")]
#[test]
fn missing_files_are_optional_and_other_read_failures_are_visible() {
    let f = Fixture::new("read");
    let path = f.0.join("settings.json");
    assert!(Input::read(Layer::Local, &path).expect("absent").is_none());
    std::fs::write(&path, b"{not JSON").expect("malformed");
    assert_eq!(
        Input::read(Layer::Local, &path)
            .expect_err("malformed")
            .kind,
        ErrorKind::Invalid
    );
    std::fs::write(&path, b"{}").expect("valid");
    let loaded = Input::read(Layer::Local, &path)
        .expect("read")
        .expect("present");
    assert_eq!(loaded.origin, Origin::file(Layer::Local, &path));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open");
    file.set_len(MAX_BYTES + 1).expect("large");
    assert_eq!(
        Input::read(Layer::Local, &path).expect_err("large").kind,
        ErrorKind::TooLarge
    );
    std::fs::remove_file(&path).expect("clear");
    std::fs::create_dir(&path).expect("directory");
    assert_eq!(
        Input::read(Layer::Local, &path)
            .expect_err("directory")
            .kind,
        ErrorKind::Invalid
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn final_links_and_special_files_cannot_supply_a_layer() {
    let f = Fixture::new("links");
    let target = f.write("target.json", b"{\"model\":\"private\"}");
    let path = f.0.join("settings.json");
    std::os::unix::fs::symlink(&target, &path).expect("link");
    assert!(Input::read(Layer::User, &path).is_err());
    std::fs::remove_file(&target).expect("dangling");
    assert!(Input::read(Layer::User, &path).is_err());
    std::fs::remove_file(&path).expect("unlink");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo")
            .success()
    );
    assert_eq!(
        Input::read(Layer::User, &path).expect_err("FIFO").kind,
        ErrorKind::Invalid
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn discovery_shares_the_local_file_between_main_and_linked_checkouts() {
    let f = Fixture::new("worktrees");
    f.repository("main/.git");
    std::fs::create_dir_all(f.0.join("main/.git/worktrees/worker")).expect("main");
    f.write("main/.git/worktrees/worker/commondir", "../..\n");
    f.write(
        "main/.git/worktrees/worker/HEAD",
        "ref: refs/heads/worker\n",
    );
    f.write(
        "worker/.git",
        format!(
            "gitdir: {}\n",
            f.0.join("main/.git/worktrees/worker").display()
        ),
    );
    std::fs::create_dir_all(f.0.join("worker/sub")).expect("nested");
    let home = f.0.join("home");
    let main = Paths::discover(
        &f.0.join("main"),
        Environment {
            home: Some(&home),
            ..Environment::default()
        },
    )
    .expect("main paths");
    let worker = Paths::discover(
        &f.0.join("worker"),
        Environment {
            home: Some(&home),
            ..Environment::default()
        },
    )
    .expect("worker paths");
    let nested = Paths::discover(
        &f.0.join("worker/sub"),
        Environment {
            home: Some(&home),
            ..Environment::default()
        },
    )
    .expect("nested paths");
    assert_eq!(main.local, worker.local);
    assert_eq!(worker.local, nested.local);
    assert_eq!(
        main.local,
        Some(f.0.join("main/.git/mdkg/config.local.json"))
    );
    assert_eq!(main.user, Some(home.join(".config/mdkg/config.json")));
    assert_ne!(main.project, worker.project);
    let layers = worker.read().expect("empty layers");
    assert!(layers.user.is_none() && layers.project.is_none() && layers.local.is_none());
    assert!(
        !home.exists() && !f.0.join("main/.git/mdkg").exists(),
        "pure reads"
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn file_inputs_resolve_in_precedence_order_without_writes() {
    let f = Fixture::new("file-order");
    f.repository("project/.git");
    let home = f.0.join("home");
    let user = f.write("home/.config/mdkg/config.json", br#"{"a":1,"b":1,"c":1}"#);
    let project = f.write("project/.mdkg/config.json", br#"{"b":2,"c":2}"#);
    let local = f.write("project/.git/mdkg/config.local.json", br#"{"c":3}"#);
    let before = [&user, &project, &local].map(|p| std::fs::read(p).expect("before"));
    let paths = Paths::discover(
        &f.0.join("project"),
        Environment {
            home: Some(&home),
            ..Environment::default()
        },
    )
    .expect("paths");
    let out = paths
        .read()
        .expect("layers")
        .resolve(&input(Layer::BuiltIn, json!({})), &[], None)
        .expect("resolve");
    assert_eq!(out.values, json!({"a":1,"b":2,"c":3}));
    for (pointer, path) in [("/a", &user), ("/b", &project), ("/c", &local)] {
        assert_eq!(out.origins[pointer].path.as_ref(), Some(path));
    }
    assert_eq!(
        [&user, &project, &local].map(|p| std::fs::read(p).expect("after")),
        before
    );
    std::fs::write(&local, b"{broken").expect("bad higher layer");
    assert!(
        paths.read().is_err(),
        "invalid high layer must not expose a lower winner"
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn no_repository_means_no_local_layer_but_bad_metadata_is_not_absence() {
    let f = Fixture::new("not-git");
    assert!(
        Paths::discover(
            &f.0,
            Environment {
                home: Some(Path::new("relative-home")),
                ..Environment::default()
            }
        )
        .is_err()
    );
    let paths = Paths::discover(&f.0, Environment::default()).expect("outside repo");
    assert!(paths.local.is_none() && paths.user.is_none());
    f.write(".git", "gitdir:missing separator\n");
    assert!(Paths::discover(&f.0, Environment::default()).is_err());
}

#[test]
fn nesting_limit_is_checked_for_parsed_and_in_memory_inputs() {
    let mut value = json!(null);
    for _ in 0..MAX_DEPTH {
        value = json!({"child":value});
    }
    let bytes = serde_json::to_vec(&value).expect("boundary JSON");
    Input::parse(Origin::value(Layer::Local), &bytes).expect("at the depth limit");
    let deeper = json!({"child":value});
    let bytes = serde_json::to_vec(&deeper).expect("deep JSON");
    assert!(Input::parse(Origin::value(Layer::Local), &bytes).is_err());
    assert!(Input::new(Origin::value(Layer::Local), deeper).is_err());
    assert_eq!(
        Input::parse(
            Origin::value(Layer::User),
            &vec![b' '; MAX_BYTES as usize + 1]
        )
        .expect_err("byte cap")
        .kind,
        ErrorKind::TooLarge
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn a_nested_checkout_uses_its_own_common_directory() {
    let f = Fixture::new("nested");
    f.repository(".git");
    f.repository("nested/.git");
    let nested = Paths::discover(&f.0.join("nested"), Environment::default()).expect("nested");
    assert_eq!(
        nested.local,
        Some(f.0.join("nested/.git/mdkg/config.local.json"))
    );
    std::fs::remove_dir_all(f.0.join("nested/.git")).expect("remove nested");
    f.write("nested/.git", "not a gitdir file\n");
    assert!(
        Paths::discover(&f.0.join("nested"), Environment::default()).is_err(),
        "can't fall back to outer repo"
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn checkout_metadata_refuses_empty_and_arbitrary_head_contents() {
    let f = Fixture::new("head-refusal");
    f.repository(".git");
    for text in [
        "",
        "anything\n",
        "ref: HEAD\n",
        "ref: refs/heads/\n",
        "ref: refs/heads/a..b\n",
        "ref: refs/heads/.hidden\n",
        "ref: refs/heads/main.lock\n",
        "ref: refs/heads/main.lock/topic\n",
        "ref: refs/heads/main.\n",
        "ref: refs/heads/a@{b\n",
        "ref: refs/heads/a b\n",
        "ref: refs//main\n",
        "ref: refs/heads/main\nextra\n",
        "ref: refs/heads/main\r\n",
        "0123\n",
    ] {
        f.write(".git/HEAD", text);
        let error = git::discover(&f.0).expect_err("invalid HEAD");
        assert!(error.contains("invalid HEAD"), "{text:?}: {error}");
        assert!(error.contains("mdkg needs a checkout with .mdkg/"));
    }
    for c in [
        ' ', '\t', '\0', '\u{7f}', '~', '^', ':', '?', '*', '[', '\\',
    ] {
        f.write(".git/HEAD", format!("ref: refs/heads/a{c}b\n"));
        assert!(
            git::discover(&f.0)
                .expect_err("invalid ref byte")
                .contains("invalid HEAD")
        );
    }
    f.write(".git/HEAD", [0xff]);
    assert!(
        git::discover(&f.0)
            .expect_err("non-UTF8 HEAD")
            .contains("invalid HEAD")
    );
    for text in [
        "a".repeat(39),
        "a".repeat(41),
        "a".repeat(63),
        "a".repeat(65),
        "A".repeat(40),
        "g".repeat(64),
        format!("{}\n\n", "a".repeat(40)),
    ] {
        f.write(".git/HEAD", text);
        assert!(
            git::discover(&f.0)
                .expect_err("invalid detached HEAD")
                .contains("invalid HEAD")
        );
    }
}

#[cfg(feature = "settings-fs")]
#[test]
fn unborn_symbolic_and_detached_heads_need_no_existing_objects() {
    let f = Fixture::new("head-valid");
    f.repository(".git");
    for head in [
        "ref: refs/heads/unborn/topic".to_string(),
        "ref: refs/heads/main\n".to_string(),
        "ref: refs/heads/λ-topic\n".to_string(),
        "0".repeat(40),
        format!("{}\n", "a".repeat(40)),
        "0123456789abcdef".repeat(4),
        format!("{}\n", "a".repeat(64)),
    ] {
        f.write(".git/HEAD", head);
        let repo = git::discover(&f.0).expect("valid HEAD").expect("checkout");
        assert_eq!(repo.common, f.0.join(".git"));
    }
    assert_eq!(
        std::fs::read_dir(f.0.join(".git/objects"))
            .expect("empty objects")
            .count(),
        0
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn working_tree_head_and_commondir_names_do_not_hide_checkout_metadata() {
    let f = Fixture::new("ordinary-names");
    f.repository(".git");
    for prefix in ["", "sub/"] {
        f.write(&format!("{prefix}HEAD"), "ordinary working file\n");
        f.write(&format!("{prefix}commondir"), "another ordinary file\n");
    }
    // These are files, not the metadata directories Git requires.
    f.write("sub/objects", "ordinary file\n");
    f.write("sub/refs", "ordinary file\n");
    let top = git::discover(&f.0)
        .expect("checkout despite filenames")
        .expect("repo");
    let sub = git::discover(&f.0.join("sub"))
        .expect("subfolder despite filenames")
        .expect("repo");
    assert_eq!(top.common, f.0.join(".git"));
    assert_eq!(top.common, sub.common);
}

#[cfg(feature = "settings-fs")]
#[test]
fn bare_metadata_never_falls_through_to_an_enclosing_checkout() {
    let f = Fixture::new("bare");
    f.repository(".git");
    let bare = f.0.join("nested.git");
    std::fs::create_dir_all(bare.join("objects")).expect("objects");
    std::fs::create_dir_all(bare.join("refs")).expect("refs");
    f.write("nested.git/HEAD", "ref: refs/heads/main\n");
    f.write("nested.git/config", "[core]\n bare = true\n");
    std::fs::create_dir_all(bare.join("sub")).expect("subfolder");
    for root in [&bare, &bare.join("sub")] {
        let error = git::discover(root).expect_err("metadata is not a checkout");
        assert!(
            error.contains("mdkg needs a checkout with .mdkg/"),
            "{error}"
        );
        let error = Paths::discover(root, Environment::default()).expect_err("bare settings");
        assert!(
            error
                .to_string()
                .contains("mdkg needs a checkout with .mdkg/")
        );
    }
    std::fs::remove_dir_all(f.0.join(".git")).expect("no enclosing checkout");
    assert!(git::discover(&bare).is_err(), "standalone bare repo");
    f.write("checkout/.git", "gitdir: ../nested.git\n");
    let error = git::discover(&f.0.join("checkout")).expect_err("bare gitdir");
    assert!(
        error.contains("mdkg needs a checkout with .mdkg/"),
        "{error}"
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn metadata_without_a_worktree_is_refused_but_a_bare_repos_linked_checkout_works() {
    let f = Fixture::new("bare-linked");
    f.repository("bare.git");
    f.write("bare.git/config", "[core]\n bare = true\n");
    for name in ["main", "worker"] {
        f.write(&format!("{name}/.git"), "gitdir: ../bare.git\n");
        assert!(
            git::discover(&f.0.join(name))
                .expect_err("no worktree")
                .contains("mdkg needs a checkout with .mdkg/")
        );
    }
    f.write("bare.git/worktrees/worker/commondir", "../..\n");
    f.write("bare.git/worktrees/worker/HEAD", "ref: refs/heads/worker\n");
    f.write("worker/.git", "gitdir: ../bare.git/worktrees/worker\n");
    let worker = git::discover(&f.0.join("worker"))
        .expect("linked checkout")
        .expect("repo");
    assert_eq!(worker.common, f.0.join("bare.git"));
    assert_eq!(worker.top, f.0.join("worker"));
    let error =
        git::discover(&worker.common.join("worktrees/worker")).expect_err("metadata directory");
    assert!(error.contains("mdkg needs a checkout with .mdkg/"));
    f.repository("separate.git");
    f.write("separate/.git", "gitdir: ../separate.git\n");
    f.write(
        "separate.git/config",
        "[core]\n bare = false\n worktree = ../missing\n",
    );
    let error = git::discover(&f.0.join("separate")).expect_err("missing worktree");
    assert!(error.contains("mdkg needs a checkout with .mdkg/"));
    f.write(
        "separate.git/config",
        "[core]\n bare = false\n worktree = \"../separate\"\n",
    );
    assert!(
        git::discover(&f.0.join("separate"))
            .expect("separate gitdir")
            .is_some()
    );
    f.write(
        "separate.git/config",
        "[core]\n bare = false\n worktree = ../worker\n",
    );
    assert!(
        git::discover(&f.0.join("separate")).is_err(),
        "not another folder's checkout"
    );
    f.write("damaged/.git", "gitdir: ../missing\n");
    let error = git::discover(&f.0.join("damaged")).expect_err("missing metadata");
    assert!(error.contains("mdkg needs a checkout with .mdkg/"));
}

#[cfg(feature = "settings-fs")]
#[test]
fn user_path_selection_needs_no_existing_home_or_repository() {
    let fixture = Fixture::new("user-path-only");
    let home = fixture.0.join("uncreated-home");
    let env = Environment {
        home: Some(&home),
        ..Environment::default()
    };
    assert_eq!(
        Paths::user_path(env).expect("pure selection"),
        Some(home.join(".config/mdkg/config.json"))
    );
    assert!(Paths::user_path(Environment::default()).unwrap().is_none());
    assert!(!home.exists());
    for invalid in ["", "relative"] {
        assert!(
            Paths::user_path(Environment {
                home: Some(Path::new(invalid)),
                ..Environment::default()
            })
            .is_err()
        );
    }
}

#[cfg(feature = "settings-fs")]
#[test]
fn user_path_prefers_absolute_xdg_then_home_without_ambient_reads() {
    let f = Fixture::new("xdg");
    let home = f.0.join("home");
    let xdg = f.0.join("config");
    let discover = |home, xdg_config_home| {
        Paths::discover(
            &f.0,
            Environment {
                home,
                xdg_config_home,
                git: None,
            },
        )
        .expect("paths")
    };
    for xdg_config_home in [None, Some(Path::new("")), Some(Path::new("relative"))] {
        assert_eq!(
            discover(Some(&home), xdg_config_home).user,
            Some(home.join(".config/mdkg/config.json"))
        );
        assert!(discover(None, xdg_config_home).user.is_none());
    }
    for home in [
        None,
        Some(home.as_path()),
        Some(Path::new("unused-relative-home")),
    ] {
        assert_eq!(
            discover(home, Some(&xdg)).user,
            Some(xdg.join("mdkg/config.json"))
        );
    }
    assert!(!home.exists() && !xdg.exists());
    f.write("home/.config/mdkg/config.json", "not valid JSON\n");
    let user = f.write("config/mdkg/config.json", br#"{"a":"xdg"}"#);
    let layer = discover(Some(&home), Some(&xdg))
        .read()
        .expect("only XDG read")
        .user
        .expect("present");
    assert_eq!(layer.value, json!({"a":"xdg"}));
    assert_eq!(layer.origin.path, Some(user));
    assert!(discover(Some(&home), None).read().is_err());
    assert!(
        discover(Some(&home), Some(&f.0.join("absent-xdg")))
            .read()
            .expect("absent XDG file does not fall back")
            .user
            .is_none()
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn physical_bare_flags_refuse_with_git_boolean_spellings() {
    let f = Fixture::new("bare-flags");
    f.repository(".git");
    for value in [
        "true",
        "YES",
        "on",
        "1",
        "\"true\"",
        "tr\\\nue",
        "true ; comment",
    ] {
        f.write(".git/config", format!("[CORE] bare = {value}\n"));
        assert!(
            git::discover(&f.0)
                .expect_err("bare")
                .contains("mdkg needs a checkout with .mdkg/"),
            "{value}"
        );
    }
    f.write(".git/config", "[core]\n bare # implicit true\n");
    assert!(git::discover(&f.0).is_err());
    for value in ["false", "NO", "off", "0", "\"false\"", ""] {
        f.write(".git/config", format!("[core]\n bare = {value}\n"));
        assert!(git::discover(&f.0).expect("not bare").is_some(), "{value}");
    }
    f.write(
        ".git/config",
        "[core]\n bare = false\n[core]\n bare = true\n",
    );
    assert!(git::discover(&f.0).is_err(), "last physical value wins");
    f.write(".git/config", "[core]\n bare = unrecognized\n");
    assert!(git::discover(&f.0).is_err(), "can't guess a checkout");
    f.write(
        ".git/config",
        format!("# {}\n[core]\n bare = false\n", "padding".repeat(700)),
    );
    assert!(
        git::discover(&f.0)
            .expect("config beyond a gitdir file's 4KiB cap")
            .is_some()
    );
    std::fs::remove_file(f.0.join(".git/config")).expect("remove config");
    std::os::unix::fs::symlink(f.0.join("not-read"), f.0.join(".git/config")).expect("config link");
    assert!(
        git::discover(&f.0).is_err(),
        "linked topology config is not followed"
    );
}

#[cfg(feature = "settings-fs")]
#[test]
fn git_overrides_warn_without_selecting_another_store_or_reading_it() {
    let f = Fixture::new("git-env");
    f.repository("main/.git");
    f.repository("other.git");
    f.write("other.git/config", "[core]\n bare = true\n");
    f.write("other.git/mdkg/config.local.json", "not valid JSON\n");
    let root = f.0.join("main");
    let discovered = |dir, common_dir| {
        Paths::discover(
            &root,
            Environment {
                git: Some(git::Environment {
                    cwd: &f.0,
                    dir,
                    common_dir,
                }),
                ..Environment::default()
            },
        )
        .expect("physical checkout wins")
    };
    let paths = discovered(Some(Path::new("other.git")), None);
    assert_eq!(paths.local, Some(root.join(".git/mdkg/config.local.json")));
    assert!(
        paths
            .read()
            .expect("foreign config not read")
            .local
            .is_none()
    );
    assert_eq!(paths.warnings.len(), 1);
    let warning = &paths.warnings[0];
    assert_eq!(warning.variable, git::Variable::Dir);
    assert_eq!(warning.resolved, Some(f.0.join("other.git")));
    assert_eq!(warning.selected, Some(root.join(".git")));
    assert_eq!(
        warning.to_string(),
        format!(
            "GIT_DIR points elsewhere; mdkg uses {:?}",
            root.join(".git")
        )
    );
    let both = discovered(Some(Path::new("other.git")), Some(Path::new("other.git")));
    assert_eq!(both.warnings.len(), 2);
    assert_eq!(both.warnings[1].variable, git::Variable::CommonDir);
    assert!(
        discovered(Some(Path::new("main/.git")), Some(Path::new("main/.git")))
            .warnings
            .is_empty()
    );
    assert!(discovered(None, None).warnings.is_empty());
    f.write("main/.git/worktrees/worker/commondir", "../..\n");
    f.write(
        "main/.git/worktrees/worker/HEAD",
        "ref: refs/heads/worker\n",
    );
    assert!(
        discovered(Some(Path::new("main/.git/worktrees/worker")), None)
            .warnings
            .is_empty(),
        "a different gitdir can name the same common dir"
    );
    std::os::unix::fs::symlink(root.join(".git"), f.0.join("same-common")).expect("alias");
    assert!(
        discovered(
            Some(Path::new("same-common")),
            Some(Path::new("same-common"))
        )
        .warnings
        .is_empty()
    );
    let outside = Paths::discover(
        &f.0,
        Environment {
            git: Some(git::Environment {
                cwd: &f.0,
                dir: Some(Path::new("other.git")),
                common_dir: None,
            }),
            ..Environment::default()
        },
    )
    .expect("outside checkout");
    assert!(outside.local.is_none());
    assert_eq!(outside.warnings.len(), 1);
    assert!(outside.warnings[0].selected.is_none());
    for path in ["", "missing", "other.git/config"] {
        let paths = discovered(Some(Path::new(path)), Some(Path::new(path)));
        assert_eq!(paths.warnings.len(), 2);
        assert!(paths.warnings.iter().all(|w| w.resolved.is_none()));
    }
}

#[test]
fn versioned_defaults_are_valid_and_layers_can_be_absent() {
    assert_eq!(DEFAULTS_VERSION, env!("CARGO_PKG_VERSION"));
    let defaults = defaults().expect("shipped defaults");
    assert_eq!(defaults.origin, Origin::value(Layer::BuiltIn));
    let out = empty_layers()
        .resolve(&defaults, &[], None)
        .expect("defaults alone");
    assert_eq!(out.values, defaults.value);
    assert!(
        out.origins
            .values()
            .all(|o| o.layer == Layer::BuiltIn && o.path.is_none())
    );
    for forbidden in [
        "schema_version",
        "format_version",
        "graph_id",
        "workspaces",
        "projections",
    ] {
        assert!(
            out.values.get(forbidden).is_none(),
            "context-dependent {forbidden}"
        );
    }
    assert!(out.values.pointer("/work/location").is_none());
    assert!(out.values.pointer("/work/committed").is_none());
}

#[test]
fn identity_is_project_only_and_refusals_name_the_source_without_values() {
    for layer in [Layer::BuiltIn, Layer::User, Layer::Local, Layer::Flags] {
        for key in ["schema_version", "format_version", "graph_id"] {
            let path = PathBuf::from("temporary-home/.config/mdkg/config.json");
            let mut value = json!({});
            value[key] = json!("IDENTITY_SENTINEL");
            let error = Input::new(Origin::file(layer, &path), value.clone())
                .expect_err("project-only identity");
            assert_eq!(error.kind, ErrorKind::Invalid);
            assert_eq!(error.origin.path.as_ref(), Some(&path));
            assert!(error.to_string().contains("config.json") && error.to_string().contains(key));
            assert!(!error.to_string().contains("IDENTITY_SENTINEL"));
            Input::new(Origin::file(Layer::Project, ".mdkg/config.json"), value)
                .expect("project identity");
        }
    }
}

#[test]
fn invalid_defaults_and_mislabelled_layers_cannot_make_an_effective_result() {
    let mut bad = input(Layer::BuiltIn, json!({}));
    bad.value = json!([]);
    assert!(Effective::new(&bad).is_err());
    let defaults = input(Layer::BuiltIn, json!({}));
    let mut layers = empty_layers();
    layers.user = Some(input(
        Layer::Project,
        json!({"graph_id":"must not sneak through"}),
    ));
    assert!(layers.resolve(&defaults, &[], None).is_err());
    layers.user = None;
    assert!(
        layers
            .resolve(&defaults, &[input(Layer::Flags, json!({}))], None)
            .is_err()
    );
    assert!(
        layers
            .resolve(
                &defaults,
                &[input(
                    Layer::Project,
                    json!({"graph_id":"definition cannot change it"})
                )],
                None
            )
            .is_err()
    );
}
