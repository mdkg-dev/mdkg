//! Execute the published setup recipe, not a second copy of its commands.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.0.wait();
    }
}

/// A scratch folder with `repo`, `home` and a `bin` holding the candidate
/// CLI as `mdkg`, and the fixture PATH that puts it first.
fn fixture(name: &str) -> (Scratch, std::ffi::OsString) {
    let dir = std::env::temp_dir().join(format!("mdkg-{name}-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&dir).expect("scratch");
    let scratch = Scratch(fs::canonicalize(dir).expect("canonical scratch"));
    let bin = scratch.0.join("bin");
    for path in [&scratch.0.join("repo"), &scratch.0.join("home"), &bin] {
        fs::create_dir(path).expect("fixture directory");
    }
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_mdkg"), bin.join("mdkg"))
        .expect("candidate CLI on fixture PATH");
    // Preserve the runner's Git selection, including Homebrew on macOS.
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").expect("runner PATH"),
    )))
    .expect("fixture PATH");
    (scratch, path)
}

/// Runs `script` with `sh -c` in `at`, isolated from the user's config,
/// within 60 seconds; its exit status and combined output.
fn sh(
    scratch: &Scratch,
    path: &std::ffi::OsStr,
    at: &std::path::Path,
    script: &str,
) -> (bool, String) {
    let home = scratch.0.join("home");
    let stdout = scratch.0.join("stdout");
    let stderr = scratch.0.join("stderr");
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../mdkg-run/tests/fixtures/fake-claude.sh");
    let mut child = Child(
        Command::new("/bin/sh")
            .args(["-c", script])
            .current_dir(at)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("MDKG_RUN_CLAUDE", fixture)
            .env("MDKG_RUN_SECURITY", "/usr/bin/false")
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout).expect("stdout"))
            .stderr(fs::File::create(&stderr).expect("stderr"))
            .process_group(0)
            .spawn()
            .expect("run script"),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll script") {
            break status;
        }
        assert!(Instant::now() < deadline, "script exceeded 60 seconds");
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = format!(
        "{}\n{}",
        fs::read_to_string(stdout).expect("stdout"),
        fs::read_to_string(stderr).expect("stderr")
    );
    (status.success(), output)
}

/// The guide's setup example: the `sh` block of its Start agents section.
fn setup_example(guide: &str) -> String {
    guide
        .split_once("## Start agents\n")
        .expect("start section")
        .1
        .split_once("```sh\n")
        .expect("setup example")
        .1
        .split_once("\n```")
        .expect("end of example")
        .0
        .to_owned()
}

/// Ref mode's guide is its own: it never sends a ref-mode user to a graph
/// checkout, a `.mdkg` to commit, or a merge into main; and its example,
/// run verbatim, publishes the definition to the graph ref and launches
/// from the code checkout. Install's note names the guide where it is.
#[test]
fn ref_mode_readme_describes_ref_mode() {
    let (scratch, path) = fixture("ref-guide");
    let repo = scratch.0.join("repo");
    let init = "set -eu\ngit init -q -b main\ngit config user.name Test\ngit config user.email test@example.invalid\ngit commit -q --allow-empty -m code\n\
                git config mdkg.graph.ref refs/heads/mdkg-graph\nmdkg init --work local\n\
                git show refs/heads/mdkg-graph:.mdkg/README.md > guide\n";
    let (ok, output) = sh(&scratch, &path, &repo, init);
    assert!(ok, "{output}");
    assert!(
        output.contains("committed .mdkg/README.md to refs/heads/mdkg-graph"),
        "{output}"
    );
    assert!(!output.contains("wrote .mdkg"), "{output}");
    assert!(
        output.contains("git show refs/heads/mdkg-graph:.mdkg/README.md"),
        "{output}"
    );
    assert!(
        !repo.join(".mdkg").exists(),
        "ref-mode init writes no checkout file"
    );
    let guide = fs::read_to_string(repo.join("guide")).expect("guide on the ref");
    for checkout_only in [
        "--root .worktrees/graph",
        "git add .mdkg",
        "squash or rebase\nmerge",
        "Use a merge commit",
        "Edit canonical skills",
        "mdkg skills sync\n",
        "Skills[mdkg skills sync]",
        "mdkg skills add mdkg-workflow",
    ] {
        assert!(
            !guide.contains(checkout_only),
            "ref-mode guide has {checkout_only:?}"
        );
    }
    // The integration recipe, as written, mirrors the skills from the
    // graph ref (`skills sync` would remove them here).
    let integrations = guide
        .split_once("For all integrations together:\n\n```sh\n")
        .expect("integration recipe")
        .1
        .split_once("\n```")
        .expect("end of recipe")
        .0;
    let (ok, output) = sh(
        &scratch,
        &path,
        &repo,
        &format!("set -eu\n{integrations}\n"),
    );
    assert!(ok, "{output}");
    // The AGENTS.md it installs names the guide where ref mode keeps it.
    let agents = fs::read_to_string(repo.join("AGENTS.md")).expect("AGENTS.md");
    assert!(
        agents.contains("git show <graph ref>:.mdkg/README.md"),
        "{agents}"
    );
    assert!(
        agents.contains("`mdkg install` refreshes the mirrors"),
        "{agents}"
    );
    // Skills sync and add refuse here, before writing anything.
    let (ok, output) = sh(
        &scratch,
        &path,
        &repo,
        "set -eu\nfor c in 'skills sync' 'skills add mdkg-workflow'; do\n\
         if mdkg $c; then exit 1; fi\ndone\nmdkg skills sync --json || true\n",
    );
    assert!(ok, "{output}");
    assert!(output.contains("skill.ref_mode"), "{output}");
    assert_eq!(
        output
            .matches("`mdkg install` refreshes the skill mirrors")
            .count(),
        3,
        "{output}"
    );
    for mirror in [".agents/skills", ".claude/skills"] {
        assert!(
            repo.join(mirror).join("mdkg-workflow/SKILL.md").is_file(),
            "{mirror}: {output}"
        );
    }
    // Validate checks the checkout's mirrors against the graph ref's skills.
    let (ok, output) = sh(&scratch, &path, &repo, "mdkg skills validate --json");
    assert!(ok, "{output}");
    assert!(!output.contains("skill.mirror"), "{output}");
    // A changed mirror is drift that install repairs from the graph ref, and
    // a mirror folder linked to the other one is a mirror.
    let (ok, output) = sh(
        &scratch,
        &path,
        &repo,
        "set -eu\nf=.claude/skills/mdkg-workflow/SKILL.md\nchmod u+w \"$f\"\necho changed >> \"$f\"\n\
         mdkg skills validate --json || true\nmdkg install\n\
         cmp \"$f\" .agents/skills/mdkg-workflow/SKILL.md\n\
         rm -r .claude/skills\nln -s ../.agents/skills .claude/skills\n\
         echo CLEAN >&2\nmdkg skills validate --json >&2\n\
         cp -R .agents/skills elsewhere\nrm .claude/skills\nln -s ../elsewhere .claude/skills\n\
         echo FOREIGN >&2\nmdkg skills validate --json >&2 || true\n\
         rm .claude/skills\nrm -rf elsewhere\nmdkg install >/dev/null\n",
    );
    assert!(ok, "{output}");
    let (damaged, rest) = output.split_once("CLEAN").expect("both runs");
    let (clean, foreign) = rest.split_once("FOREIGN").expect("foreign run");
    assert!(damaged.contains("skill.mirror_drift"), "{output}");
    assert!(damaged.contains("run `mdkg install`"), "{output}");
    assert!(damaged.contains("commit or set those aside"), "{output}");
    assert!(!damaged.contains("edit the .mdkg/skills"), "{output}");
    assert!(!clean.contains("skill.mirror"), "{output}");
    assert!(foreign.contains("isn't another mirror"), "{output}");
    // After a --graph-only init, skills add refuses before it writes the
    // skill anywhere: not on the graph ref, not in the graph cache.
    let (ok, output) = sh(
        &scratch,
        &path,
        &scratch.0,
        "set -eu\ngit init -q -b main bare-skills\ncd bare-skills\ngit config user.name Test\n\
         git config user.email test@example.invalid\ngit commit -q --allow-empty -m code\ngit config mdkg.graph.ref refs/heads/mdkg-graph\n\
         mdkg init --work local --graph-only\n\
         if mdkg skills add mdkg-workflow; then exit 1; fi\n\
         if git cat-file -e refs/heads/mdkg-graph:.mdkg/skills/mdkg-workflow/SKILL.md; then exit 1; fi\n\
         test -z \"$(find .git -path '*skills/mdkg-workflow*')\"\n",
    );
    assert!(ok, "{output}");
    assert!(output.contains("skill.ref_mode"), "{output}");
    let commands = setup_example(&guide);
    assert!(
        commands
            .lines()
            .last()
            .unwrap()
            .ends_with("agents up worker")
    );
    let script = format!(
        "set -eu\nmdkg --as guide-check msg send --to worker --kind note --text 'Guide check'\n\
         {commands} --once\n\
         git show refs/heads/mdkg-graph:.mdkg/agents/worker.json > committed-definition\n\
         git status --porcelain --untracked-files=no > code-changes\n\
         git log --format=%s main > code-log\n"
    );
    let (ok, output) = sh(&scratch, &path, &repo, &script);
    assert!(ok, "{output}");
    // Install names the guide by a command that works here.
    let note = "`git show refs/heads/mdkg-graph:.mdkg/README.md`";
    assert!(output.contains(note), "{output}");
    assert!(!output.contains("See .mdkg/README.md"), "{output}");
    assert!(
        fs::read_to_string(repo.join("committed-definition"))
            .expect("definition on the graph ref")
            .contains("claude")
    );
    assert_eq!(
        fs::read_to_string(repo.join("code-log")).expect("code log"),
        "code\n",
        "the guide's steps commit nothing to the code branch"
    );
    assert!(
        repo.join(".worktrees/worker/claude.args").is_file(),
        "recipe must reach the fake harness: {output}"
    );
}

/// A clone of a ref-mode repository that fetched the graph ref: commands
/// that find no config point to adoption, init refuses to start a second
/// graph (unless asked to), and the named install adopts it.
#[test]
fn clone_with_fetched_graph_ref_points_to_adoption() {
    let (scratch, path) = fixture("ref-clone");
    let top = scratch.0.clone();
    let setup = "set -eu\ngit init -q --bare remote.git\ngit init -q -b main repo\ncd repo\n\
                 git config user.name Test\ngit config user.email test@example.invalid\ngit commit -q --allow-empty -m code\ngit remote add origin ../remote.git\n\
                 git config mdkg.graph.ref refs/heads/mdkg-graph\nmdkg init --work local\n\
                 git push -q origin main mdkg-graph\ncd ..\ngit clone -q remote.git clone\ncd clone\ngit config user.name Test\ngit config user.email test@example.invalid\n";
    let (ok, output) = sh(&scratch, &path, &top, setup);
    assert!(ok, "{output}");
    let clone = top.join("clone");
    let adopt = "run `mdkg install --setup personal`";
    for command in ["mdkg list", "mdkg status", "mdkg doctor"] {
        let (ok, output) = sh(&scratch, &path, &clone, command);
        assert!(!ok, "{command}: {output}");
        assert!(output.contains(adopt), "{command}: {output}");
        assert!(
            output.contains("refs/remotes/origin/mdkg-graph"),
            "{command}: {output}"
        );
    }
    // Neither mode's init forks the graph.
    for script in [
        "mdkg init --work local",
        "mdkg init --work local --json",
        "git config mdkg.graph.ref refs/heads/mdkg-graph && mdkg init --work local; \
         s=$?; git config --unset mdkg.graph.ref; exit $s",
    ] {
        let (ok, output) = sh(&scratch, &path, &clone, script);
        assert!(!ok, "{script}: {output}");
        assert!(output.contains("init.graph_fetched"), "{script}: {output}");
        assert!(
            output.contains("install --setup personal"),
            "{script}: {output}"
        );
        assert!(
            !clone.join(".mdkg").exists(),
            "{script}: init wrote a graph"
        );
        let (_, local) = sh(
            &scratch,
            &path,
            &clone,
            "git rev-parse -q --verify refs/heads/mdkg-graph",
        );
        assert!(
            local.trim().is_empty(),
            "{script}: init made a graph ref: {local}"
        );
    }
    // Any other fetched graph fails closed too, with install's help in
    // place of a command whose setup might not fit it.
    let generic = "adopt it with `mdkg install --setup <personal|professional>`";
    let (ok, output) = sh(
        &scratch,
        &path,
        &clone,
        "set -eu\ngit update-ref refs/remotes/no/such/mdkg-graph refs/remotes/origin/mdkg-graph\n\
         git update-ref -d refs/remotes/origin/mdkg-graph\nmdkg list || true\n\
         if mdkg init --work local; then exit 1; fi\ntest ! -e .mdkg\n\
         git update-ref -d refs/remotes/no/such/mdkg-graph",
    );
    assert!(ok, "{output}");
    assert!(
        output.contains("refs/remotes/no/such/mdkg-graph"),
        "{output}"
    );
    assert!(output.contains(generic), "{output}");
    assert!(output.contains("init.graph_fetched"), "{output}");
    assert!(!output.contains("or run `mdkg init`"), "{output}");
    assert!(!output.contains("--setup personal`"), "{output}");
    // Another remote's graph gets the same hint.
    let (ok, output) = sh(
        &scratch,
        &path,
        &top,
        "set -eu\ngit clone -q --bare remote.git graph.git\ncd clone\n\
         git remote add mine ../graph.git\ngit fetch -q mine\nmdkg list || true",
    );
    assert!(ok, "{output}");
    assert!(output.contains("refs/remotes/mine/mdkg-graph"), "{output}");
    assert!(output.contains(generic), "{output}");
    assert!(!output.contains("--graph-remote"), "{output}");
    // A graph remote whose host made `mdkg-graph` its default branch: a
    // graph-only default is a graph ref, never code to merge.
    let (ok, output) = sh(
        &scratch,
        &path,
        &clone,
        "set -eu\ngit symbolic-ref refs/remotes/mine/HEAD refs/remotes/mine/mdkg-graph\n\
         mdkg list || true\nif mdkg init --work local; then exit 1; fi\ntest ! -e .mdkg\n\
         git symbolic-ref -d refs/remotes/mine/HEAD",
    );
    assert!(ok, "{output}");
    assert!(output.contains("refs/remotes/mine/mdkg-graph"), "{output}");
    assert!(
        output.contains("switch to or merge your team's default branch"),
        "{output}"
    );
    assert!(!output.contains("git merge"), "{output}");
    // A personal graph ref that keeps the whole repository (G20): the hint
    // gives both routes, and with no default branch holding the graph it
    // names no branch to merge.
    let (ok, output) = sh(
        &scratch,
        &path,
        &clone,
        "set -eu\ngit fetch -q origin\ng=$(git rev-parse refs/remotes/origin/mdkg-graph)\n\
         export GIT_INDEX_FILE=.git/full-index\ngit read-tree refs/remotes/origin/main\n\
         git read-tree --prefix=.mdkg/ \"$g:.mdkg\"\nt=$(git write-tree)\nunset GIT_INDEX_FILE\n\
         rm .git/full-index\n\
         git update-ref refs/remotes/origin/mdkg-graph \"$(git commit-tree -p refs/remotes/origin/main -m full \"$t\")\"\n\
         test ! -e .mdkg\nmdkg list || true\n\
         git update-ref refs/remotes/origin/mdkg-graph \"$g\"\n",
    );
    assert!(ok, "{output}");
    assert!(output.contains(adopt), "{output}");
    assert!(
        output.contains("merge your team's default branch"),
        "{output}"
    );
    assert!(!output.contains("git merge"), "{output}");
    let (ok, output) = sh(
        &scratch,
        &path,
        &clone,
        "set -eu\ngit remote remove mine\ngit fetch -q origin\n\
         mdkg install --setup personal\nmdkg list\nmdkg status\nmdkg doctor --strict\n",
    );
    assert!(ok, "the named install adopts the graph: {output}");
    // Its graph remote isn't approved (no 0.7.1 command can), which doctor
    // reports without a warning, saying when approval arrives.
    assert!(
        output.contains("not approved: nothing is pushed to it (approving a graph remote"),
        "{output}"
    );
    // A graph published under a custom name, which the clone's config
    // names, is found as well.
    let (ok, output) = sh(
        &scratch,
        &path,
        &top,
        "set -eu\ngit -C remote.git branch team-graph mdkg-graph\ngit -C remote.git branch -D mdkg-graph\n\
         git clone -q -b main remote.git team\ncd team\n\
         git config mdkg.graph.ref refs/heads/team-graph\ngit config mdkg.graph.remote origin\n\
         git config mdkg.graph.remoteRef refs/heads/team-graph\n\
         if mdkg init --work local; then exit 1; fi\n\
         ! git rev-parse -q --verify refs/heads/team-graph\n\
         git -C ../remote.git branch mdkg-graph team-graph",
    );
    assert!(ok, "{output}");
    assert!(output.contains("init.graph_fetched"), "{output}");
    assert!(
        output.contains("refs/remotes/origin/team-graph"),
        "{output}"
    );
    // A fetched config git can't fully read fails closed: a blob a
    // partial clone hasn't fetched, a tree that isn't local, a ref name
    // that isn't UTF-8, and a git that can't run at all.
    let (ok, output) = sh(
        &scratch,
        &path,
        &top,
        "set -eu\ngit init -q -b main probe\ncd probe\ngit config user.name Test\n\
         git config user.email test@example.invalid\ngit commit -q --allow-empty -m code\n\
         refuse() { if mdkg init --work local \"$@\"; then exit 1; fi; test ! -e .mdkg; }\n\
         graph() { c=$(git commit-tree -m g \"$1\"); git update-ref refs/remotes/o/g \"$c\"; }\n\
         b=$(printf '100644 blob %040d\\tconfig.json\\n' 1 | git mktree --missing)\n\
         graph \"$(printf '040000 tree %s\\t.mdkg\\n' \"$b\" | git mktree)\"\necho CASE blob >&2\nrefuse\n\
         git update-ref refs/remotes/o/feature/HEAD refs/remotes/o/g\ngit update-ref -d refs/remotes/o/g\n\
         if out=$(mdkg init --work local 2>&1); then exit 1; fi\n\
         case $out in *'refs/remotes/o/feature/HEAD holds a graph'*) ;; *) echo \"$out\" >&2; exit 1;; esac\n\
         git update-ref refs/remotes/o/g refs/remotes/o/feature/HEAD\ngit update-ref -d refs/remotes/o/feature/HEAD\nmkdir -p sub\n(cd sub && refuse && git config mdkg.graph.ref refs/heads/mdkg-graph && refuse; \
         s=$?; git config --unset mdkg.graph.ref; exit $s)\ntest ! -e sub/.mdkg\n\
         ! git rev-parse -q --verify refs/heads/mdkg-graph\n\
         graph \"$(printf '040000 tree %s\\tsub\\n' \"$(printf '040000 tree %s\\t.mdkg\\n' \"$b\" | git mktree)\" | git mktree)\"\n\
         (cd sub && refuse)\nif mdkg init --work local --root sub; then exit 1; fi\ntest ! -e sub/.mdkg\n\
         graph \"$(printf '040000 tree %s\\t:api\\n' \"$(printf '040000 tree %s\\t.mdkg\\n' \"$b\" | git mktree)\" | git mktree)\"\n\
         named() { if out=$(\"$@\" 2>&1); then exit 1; fi; case $out in \
         *'refs/remotes/o/g holds a graph'*'install --setup'*) ;; *) echo \"$out\" >&2; exit 1;; esac; }\n\
         mkdir :api\n(cd :api && named mdkg init --work local)\nnamed mdkg init --work local --root :api\n\
         test ! -e :api/.mdkg\n\
         graph \"$(printf '040000 tree %040d\\t.mdkg\\n' 2 | git mktree --missing)\"\necho CASE tree >&2\nrefuse\n\
         git update-ref -d refs/remotes/o/g\n\
         printf '%s refs/remotes/o/a\\377\\n' \"$(git rev-parse HEAD)\" >> .git/packed-refs\n\
         echo CASE name >&2\nrefuse\nrm .git/packed-refs\n\
         mkdir fake\nprintf '#!/bin/sh\\nexit 1\\n' > fake/git\nchmod +x fake/git\n\
         echo CASE git >&2\nPATH=\"$PWD/fake:$PATH\" refuse --in-checkout\nrm -r fake\n\
         graph \"$(printf '040000 tree %s\\t.mdkg\\n' \"$(printf '040000 tree %s\\tconfig.json\\n' \"$(git mktree </dev/null)\" | git mktree)\" | git mktree)\"\n\
         echo CASE none >&2\nmdkg init --work local >&2\ntest -f .mdkg/config.json\n",
    );
    assert!(ok, "{output}");
    let cases: Vec<&str> = output.split("CASE ").skip(1).collect();
    assert_eq!(cases.len(), 5, "{output}");
    // (A git that can't run stops init before the guard; either way it
    // refused and wrote nothing.)
    for case in &cases[..3] {
        assert!(case.contains("init.graph_fetched"), "{case}");
    }
    assert!(
        cases[0].contains("refs/remotes/o/g holds a graph"),
        "{output}"
    );
    // Git couldn't say: the refusal gives both routes, naming no branch.
    for case in &cases[1..3] {
        assert!(case.contains("git couldn't list"), "{case}");
        assert!(
            case.contains("(checkout mode), switch to or merge your team's default branch"),
            "{case}"
        );
        assert!(
            case.contains("install --setup <personal|professional>"),
            "{case}"
        );
        assert!(!case.contains("git merge"), "{case}");
    }
    // `--new-graph` is the explicit way to start another one.
    let (ok, output) = sh(
        &scratch,
        &path,
        &top,
        "set -eu\ngit clone -q remote.git other\ncd other\nmdkg init --work local --new-graph\n\
         test -f .mdkg/config.json\n",
    );
    assert!(ok, "{output}");
}

/// A checkout-mode team repository: the graph merged into main, and a
/// teammate's branch predates it. The hint gives both routes, naming the
/// default branch (never another branch holding the graph) to merge, and
/// init still refuses.
#[test]
fn branch_predating_a_checkout_mode_graph_points_to_merge() {
    let (scratch, path) = fixture("checkout-clone");
    let top = scratch.0.clone();
    let setup = "set -eu\ngit init -q --bare -b main remote.git\ngit init -q -b main repo\ncd repo\n\
                 git config user.name Test\ngit config user.email test@example.invalid\n\
                 echo code > a.txt\ngit add a.txt\ngit commit -q -m code\ngit branch feature\n\
                 git remote add origin ../remote.git\nmdkg init --work local\ngit add .mdkg\n\
                 git commit -q -m graph\ngit branch bugfix\ngit push -q origin main feature bugfix\ncd ..\n\
                 git clone -q -b feature remote.git mate\n";
    let (ok, output) = sh(&scratch, &path, &top, setup);
    assert!(ok, "{output}");
    let mate = top.join("mate");
    // `origin/bugfix` holds the graph too and sorts first; the hint names
    // the default branch.
    let merge = "merge refs/remotes/origin/main (`git merge refs/remotes/origin/main`), or \
                 switch to main once it's up to date with refs/remotes/origin/main";
    for command in ["mdkg status", "mdkg init --work local"] {
        let (ok, output) = sh(&scratch, &path, &mate, command);
        assert!(!ok, "{command}: {output}");
        assert!(output.contains(merge), "{command}: {output}");
        assert!(output.contains("install --setup"), "{command}: {output}");
        assert!(
            !output.contains("git merge refs/remotes/origin/bugfix"),
            "{command}: {output}"
        );
        assert!(
            !mate.join(".mdkg").exists(),
            "{command}: init wrote a graph"
        );
    }
    let (ok, output) = sh(&scratch, &path, &mate, "mdkg init --work local --json");
    assert!(!ok, "{output}");
    assert!(output.contains("init.graph_fetched"), "{output}");
    // With `origin/mdkg-graph` fetched as well, the hint gives both routes'
    // commands: the default branch to merge, and personal setup's install.
    for command in ["mdkg status", "mdkg init --work local"] {
        let script = format!(
            "set -eu\ngit update-ref refs/remotes/origin/mdkg-graph refs/remotes/origin/bugfix\n\
             if {command}; then s=1; else s=0; fi\n\
             git update-ref -d refs/remotes/origin/mdkg-graph\nexit $s"
        );
        let (ok, output) = sh(&scratch, &path, &mate, &script);
        assert!(ok, "{command}: {output}");
        assert!(output.contains(merge), "{command}: {output}");
        assert!(
            output.contains("run `mdkg install --setup personal`"),
            "{command}: {output}"
        );
        assert!(
            !mate.join(".mdkg").exists(),
            "{command}: init wrote a graph"
        );
    }
    // Another remote's default branch is a graph-only ref whose graph
    // declares a workspace (`w/.mdkg`), and it sorts first: it isn't code
    // to merge, and the code default after it is still named.
    let script = r#"set -eu
cfg=$(git show refs/remotes/origin/main:.mdkg/config.json | awk 'NR==1{print; print "  \"workspaces\": {\"root\": {\"path\": \".\", \"enabled\": true, \"mdkg_dir\": \".mdkg\"}, \"w\": {\"path\": \"w\", \"enabled\": true, \"mdkg_dir\": \".mdkg\"}},"; next} 1')
blob=$(printf '%s\n' "$cfg" | git hash-object -w --stdin)
md=$( (git ls-tree refs/remotes/origin/main:.mdkg | grep -v '	config.json$'; printf '100644 blob %s\tconfig.json\n' "$blob") | git mktree)
x=$(echo x | git hash-object -w --stdin)
w=$(printf '040000 tree %s\t.mdkg\n' "$(printf '100644 blob %s\tx.md\n' "$x" | git mktree)" | git mktree)
t=$(printf '040000 tree %s\t.mdkg\n040000 tree %s\tw\n' "$md" "$w" | git mktree)
git update-ref refs/remotes/mine/mdkg-graph "$(git commit-tree -m g "$t")"
git symbolic-ref refs/remotes/mine/HEAD refs/remotes/mine/mdkg-graph
mdkg status || true
if mdkg init --work local; then exit 1; fi
test ! -e .mdkg
git symbolic-ref -d refs/remotes/mine/HEAD
git update-ref -d refs/remotes/mine/mdkg-graph
"#;
    let (ok, output) = sh(&scratch, &path, &mate, script);
    assert!(ok, "{output}");
    assert_eq!(output.matches(merge).count(), 2, "{output}");
    assert!(!output.contains("git merge refs/remotes/mine"), "{output}");
    // The same graph-only default in a blobless clone that hasn't fetched
    // its config: listed but unreadable, it still isn't code to merge.
    let script = r#"set -eu
md=$(printf '100644 blob %040d\tconfig.json\n' 1 | git mktree --missing)
x=$(echo x | git hash-object -w --stdin)
w=$(printf '040000 tree %s\t.mdkg\n' "$(printf '100644 blob %s\tx.md\n' "$x" | git mktree)" | git mktree)
t=$(printf '040000 tree %s\t.mdkg\n040000 tree %s\tw\n' "$md" "$w" | git mktree)
git update-ref refs/remotes/mine/mdkg-graph "$(git commit-tree -m g "$t")"
git symbolic-ref refs/remotes/mine/HEAD refs/remotes/mine/mdkg-graph
mdkg status || true
if mdkg init --work local; then exit 1; fi
test ! -e .mdkg
git symbolic-ref -d refs/remotes/mine/HEAD
git update-ref -d refs/remotes/mine/mdkg-graph
"#;
    let (ok, output) = sh(&scratch, &path, &mate, script);
    assert!(ok, "{output}");
    assert_eq!(output.matches(merge).count(), 2, "{output}");
    assert!(!output.contains("git merge refs/remotes/mine"), "{output}");
    // Following the hint adopts the team's graph in checkout mode.
    let (ok, output) = sh(
        &scratch,
        &path,
        &mate,
        "set -eu\ngit config user.name Test\ngit config user.email test@example.invalid\n\
         git merge -q --no-edit refs/remotes/origin/main\nmdkg list\n\
         test -z \"$(git config mdkg.graph.ref || true)\"\n",
    );
    assert!(ok, "{output}");
}

#[test]
fn documented_agent_setup_commits_the_definition_and_runs_from_the_graph_checkout() {
    let guide = include_str!("../src/init_readme.md");
    let commands = guide
        .split_once("## Start agents\n")
        .expect("start section")
        .1
        .split_once("```sh\n")
        .expect("setup example")
        .1
        .split_once("\n```")
        .expect("end of example")
        .0;
    // Bound the documented watcher to its documented one-pass alternative.
    // Every other command runs verbatim, including Git and install ordering.
    assert!(
        commands
            .lines()
            .last()
            .unwrap()
            .ends_with("agents up worker")
    );
    let commands = format!("{commands} --once");

    for work in ["local", "shared"] {
        let dir = std::env::temp_dir().join(format!("mdkg-init-guide-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&dir).expect("scratch");
        let scratch = Scratch(fs::canonicalize(dir).expect("canonical scratch"));
        let repo = scratch.0.join("repo");
        let home = scratch.0.join("home");
        let bin = scratch.0.join("bin");
        for path in [&repo, &home, &bin] {
            fs::create_dir(path).expect("fixture directory");
        }
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_mdkg"), bin.join("mdkg"))
            .expect("candidate CLI on fixture PATH");
        // Preserve the runner's Git selection, including Homebrew on macOS.
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("runner PATH"),
        )))
        .expect("fixture PATH");
        let stdout = scratch.0.join("stdout");
        let stderr = scratch.0.join("stderr");
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../mdkg-run/tests/fixtures/fake-claude.sh");
        // Supply eligible mail so the bounded watcher must launch its worker.
        let script = format!(
            "set -eu\ngit init -q -b main\nmdkg init --work {work}\n\
             mdkg --as guide-check msg send --to worker --kind note --text 'Guide check'\n\
             {commands}\n\
             git -C .worktrees/graph show HEAD:.mdkg/agents/worker.json > committed-definition\n\
             git -C .worktrees/graph diff-tree --no-commit-id --name-only -r HEAD > definition-paths\n"
        );
        let mut child = Child(
            Command::new("/bin/sh")
                .args(["-c", &script])
                .current_dir(&repo)
                .env_clear()
                .env("PATH", path)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .env("MDKG_RUN_CLAUDE", fixture)
                .env("MDKG_RUN_SECURITY", "/usr/bin/false")
                .stdin(Stdio::null())
                .stdout(fs::File::create(&stdout).expect("stdout"))
                .stderr(fs::File::create(&stderr).expect("stderr"))
                .process_group(0)
                .spawn()
                .expect("run recipe"),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("poll recipe") {
                break status;
            }
            assert!(Instant::now() < deadline, "recipe exceeded 60 seconds");
            std::thread::sleep(Duration::from_millis(20));
        };
        let output = format!(
            "{}\n{}",
            fs::read_to_string(stdout).expect("stdout"),
            fs::read_to_string(stderr).expect("stderr")
        );
        assert!(status.success(), "{work}: {output}");
        assert_eq!(
            fs::read(repo.join("committed-definition")).expect("committed definition"),
            fs::read(repo.join(".worktrees/graph/.mdkg/agents/worker.json"))
                .expect("authored definition"),
            "{work}: agents add must commit the exact definition"
        );
        assert_eq!(
            fs::read_to_string(repo.join("definition-paths")).expect("commit paths"),
            ".mdkg/agents/worker.json\n",
            "{work}: the definition commit must contain only the definition"
        );
        assert!(
            repo.join(".worktrees/worker/claude.args").is_file(),
            "{work}: recipe must reach the fake harness: {output}"
        );
    }
}

/// init writes the project files a repository commits with the umask's
/// mode, as git and an editor do, not 0600 (e2e S7): 0644 under 022, and
/// 0600 under 077, so the mode isn't forced either way.
#[test]
fn init_writes_committed_files_with_the_umask_s_mode() {
    use std::os::unix::fs::PermissionsExt;
    for (umask, want) in [("022", 0o644), ("077", 0o600)] {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-cli-init-modes-{umask}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let scratch = Scratch(dir.clone());
        let repo = scratch.0.join("repo");
        fs::create_dir_all(&repo).expect("repo");
        let ok = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git")
            .success();
        assert!(ok);
        // The umask in a shell of its own, so this process's umask, which
        // every test thread shares, stays as it is.
        let out = Command::new("/bin/sh")
            .args([
                "-c",
                &format!("umask {umask}; exec \"$0\" init"),
                env!("CARGO_BIN_EXE_mdkg"),
            ])
            .current_dir(&repo)
            .env("HOME", &scratch.0)
            .env("XDG_CONFIG_HOME", &scratch.0)
            .env("XDG_DATA_HOME", &scratch.0)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("MDKG_ACTOR")
            .output()
            .expect("init");
        assert!(out.status.success(), "{out:?}");
        for file in [
            ".mdkg/config.json",
            ".mdkg/README.md",
            ".mdkg/skills/mdkg-workflow/SKILL.md",
        ] {
            let mode = fs::metadata(repo.join(file))
                .expect(file)
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, want, "{file} under umask {umask}");
        }
    }
}
