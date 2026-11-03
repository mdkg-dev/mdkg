use super::*;
use crate::testutil::TempDir;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const KEY: &str = concat!(
    "ssh-ed25519 ",
    "AAAAC3NzaC1lZDI1NTE5AAAAI",
    "Ftestonlyhostkey"
);

/// A real git, with no config but the repository's, for fixtures only.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A worker repository with one commit: its common dir and the commit.
fn worker(t: &TempDir) -> (PathBuf, String) {
    let w = t.path().join("worker");
    fs::create_dir(&w).unwrap();
    git(&w, &["init", "-q", "-b", "agent/a"]);
    fs::write(w.join("f"), "x").unwrap();
    git(&w, &["add", "f"]);
    git(
        &w,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "m",
        ],
    );
    let sha = git(&w, &["rev-parse", "HEAD"]);
    (fs::canonicalize(w.join(".git")).unwrap(), sha)
}

#[test]
fn only_a_full_lowercase_sha_names_a_commit() {
    assert_eq!(check_commit(&"a".repeat(40)).unwrap(), ObjectFormat::Sha1);
    assert_eq!(check_commit(&"0".repeat(64)).unwrap(), ObjectFormat::Sha256);
    for bad in [
        "HEAD".to_owned(),
        "agent/a".to_owned(),
        "A".repeat(40),
        "a".repeat(39),
        "a".repeat(41),
        format!("{}^{{commit}}", "a".repeat(40)),
        String::new(),
    ] {
        assert_eq!(
            check_commit(&bad).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "{bad}"
        );
    }
}

#[test]
fn only_an_ssh_url_is_a_destination() {
    for ok in [
        "ssh://git@host.lan/srv/repo.git",
        "ssh://host:2222/r",
        "git@host.lan:repo.git",
        "host:path/r.git",
        "ssh://[::1]:22/r",
        "git@[fe80::1]:r.git",
        "ssh://git-user@host-1.lan:2222/srv/r",
    ] {
        check_url(ok).unwrap();
    }
    for bad in [
        "",
        "file:///tmp/r.git",
        "/local/path",
        "./r.git",
        "-oProxyCommand=x:y",
        "ext::sh -c x",
        "http://host/r.git",
        "host:",
        "a/b:c",
        "git@ host:r",
        "ssh://-oProxyCommand=x/r",
        "ssh://host",
        "git@host:r\n",
        // Terra 2 #10743: git's `<transport>::` forms.
        "ext::sh%20-c%20x",
        "fd::3",
        "host::path",
        "git@host::r",
        // A user starting with '-', a port that isn't digits.
        "ssh://-oProxyCommand=x@host/r",
        "ssh://host:-oX/r",
        "ssh://host:/r",
        "ssh://a@b@host/r",
        "-oProxyCommand=x@host:r",
        "ssh://ho$t/r",
        "ssh://[::1x]/r",
    ] {
        assert!(check_url(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_host_key_is_one_type_and_its_base64() {
    check_host_key(KEY).unwrap();
    for bad in [
        String::new(),
        format!("host {KEY}"),
        format!("{KEY}\nevil ssh-ed25519 AAAA"),
        "ssh-dss AAAA".to_owned(),
        "ssh-ed25519".to_owned(),
        "ssh-ed25519 AA AA".to_owned(),
        "ssh-ed25519 AA$A".to_owned(),
    ] {
        assert!(check_host_key(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_private_repo_is_made_whole_and_reaches_the_workers_commits() {
    let t = TempDir::new();
    let (common, sha) = worker(&t);
    let root = push_root(t.path());
    let repo = PrivateRepo::create(&root, &common, check_commit(&sha).unwrap(), KEY).unwrap();
    repo.check().unwrap();
    assert_eq!(
        fs::read(repo.git_dir().join("objects/info/alternates")).unwrap(),
        format!("{}\n", common.join("objects").display()).into_bytes()
    );
    assert_eq!(
        fs::read_to_string(repo.dir.join("known_hosts")).unwrap(),
        format!("{HOST_KEY_ALIAS} {KEY}\n")
    );
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // git takes the hand-made repository as a bare one and finds the
    // worker's commit through the alternates, never opening the worker.
    let gd = repo.git_dir();
    let gd = gd.to_str().unwrap();
    assert_eq!(
        git(
            t.path(),
            &["--git-dir", gd, "rev-parse", "--is-bare-repository"]
        ),
        "true"
    );
    assert_eq!(
        git(t.path(), &["--git-dir", gd, "cat-file", "-t", &sha]),
        "commit"
    );
    // The SHA-256 form says so in its config.
    let r256 = PrivateRepo::create(&root, &common, ObjectFormat::Sha256, KEY).unwrap();
    assert!(
        fs::read_to_string(r256.git_dir().join("config"))
            .unwrap()
            .contains("objectformat = sha256")
    );
    // Each push has its own folder, removed when it ends.
    let dir = repo.dir.clone();
    assert_ne!(dir, r256.dir);
    drop(repo);
    drop(r256);
    assert!(!dir.exists());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
}

/// A change made to a push's folder.
type Tamper = Box<dyn Fn(&Path)>;

#[test]
fn any_change_to_a_private_repo_refuses_the_push() {
    // Terra 2 #6271: a URL rewrite, a redirected git folder or objects,
    // or a swapped host key, all fail the check made just before git.
    let t = TempDir::new();
    let (common, sha) = worker(&t);
    let root = push_root(t.path());
    let format = check_commit(&sha).unwrap();
    let tampers: Vec<(&str, Tamper)> = vec![
        (
            "url rewrite",
            Box::new(|d| {
                let c = d.join("repo.git/config");
                let mut s = fs::read_to_string(&c).unwrap();
                s.push_str("[url \"ssh://evil/\"]\n\tpushInsteadOf = ssh://\n");
                fs::write(c, s).unwrap();
            }),
        ),
        (
            "commondir",
            Box::new(|d| fs::write(d.join("repo.git/commondir"), "/elsewhere\n").unwrap()),
        ),
        (
            "alternates",
            Box::new(|d| fs::write(d.join("repo.git/objects/info/alternates"), "/x\n").unwrap()),
        ),
        (
            "extra ref",
            Box::new(|d| fs::write(d.join("repo.git/refs/heads/x"), "0\n").unwrap()),
        ),
        (
            "known hosts",
            Box::new(|d| fs::write(d.join("known_hosts"), "mdkg-push ssh-rsa AAAA\n").unwrap()),
        ),
        (
            "config link",
            Box::new(|d| {
                let c = d.join("repo.git/config");
                let real = d.join("real-config");
                fs::rename(&c, &real).unwrap();
                std::os::unix::fs::symlink(&real, &c).unwrap();
            }),
        ),
        (
            "group writable",
            Box::new(|d| {
                fs::set_permissions(d.join("repo.git/config"), fs::Permissions::from_mode(0o620))
                    .unwrap()
            }),
        ),
        (
            "hooks folder",
            Box::new(|d| fs::create_dir(d.join("repo.git/hooks")).unwrap()),
        ),
    ];
    for (what, tamper) in tampers {
        let repo = PrivateRepo::create(&root, &common, format, KEY).unwrap();
        tamper(&repo.dir);
        let err = repo.check().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{what}: {err}");
        let dir = repo.dir.clone();
        drop(repo);
        assert!(!dir.exists(), "{what}");
    }
}

#[test]
fn a_bad_destination_or_root_makes_nothing() {
    let t = TempDir::new();
    let (common, _) = worker(&t);
    let root = push_root(t.path());
    assert!(PrivateRepo::create(&root, &common, ObjectFormat::Sha1, "not a key").is_err());
    assert!(!root.exists());
    // A root that's a link, or open to others, is refused.
    let elsewhere = t.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::create_dir_all(root.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
    assert!(PrivateRepo::create(&root, &common, ObjectFormat::Sha1, KEY).is_err());
    fs::remove_file(&root).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(PrivateRepo::create(&root, &common, ObjectFormat::Sha1, KEY).is_err());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn the_ssh_command_names_only_the_bindings_key_and_identity() {
    let t = TempDir::new();
    let (common, _) = worker(&t);
    let repo = PrivateRepo::create(&push_root(t.path()), &common, ObjectFormat::Sha1, KEY).unwrap();
    let cmd = repo
        .ssh_command(Path::new("/usr/bin/ssh"), Path::new("/keys/id_ed25519"))
        .unwrap();
    for part in [
        // Terra 2 #10743: ssh by absolute path, never through PATH.
        "'/usr/bin/ssh' -F none ",
        "-o BatchMode=yes",
        "-o IdentitiesOnly=yes",
        "-o IdentityFile='/keys/id_ed25519'",
        "-o StrictHostKeyChecking=yes",
        "-o GlobalKnownHostsFile=/dev/null",
        "-o HostKeyAlias=mdkg-push",
    ] {
        assert!(cmd.contains(part), "{part} in {cmd}");
    }
    let known = repo.dir.join("known_hosts");
    assert!(cmd.contains(&format!("-o UserKnownHostsFile='{}'", known.display())));
    // A relative identity, or one that could break out of its quotes.
    let ssh = Path::new("/usr/bin/ssh");
    for bad in ["keys/id", "/keys/it's", "/keys/a\nb"] {
        assert!(repo.ssh_command(ssh, Path::new(bad)).is_err(), "{bad:?}");
    }
    assert!(repo.ssh_command(Path::new("ssh"), Path::new("/k")).is_err());
}

#[test]
fn a_branch_is_a_plain_name() {
    for ok in ["agent/a", "agent/codex%3Apr19", "agent/a.g2", "main"] {
        check_branch(ok).unwrap();
    }
    for bad in [
        "",
        "-x",
        "/a",
        "a/",
        "a..b",
        "a//b",
        "a@{1}",
        "a.lock",
        "a/b.lock/c",
        "a b",
        "a~1",
        "a^",
        "a:b",
        "a?",
        "a*",
        "a[",
        "a\\b",
        ".a",
        "a/.b",
        "a.",
        "@",
    ] {
        assert!(check_branch(bad).is_err(), "{bad:?}");
    }
}

/// A fake ssh for tests: runs the remote command (`git-receive-pack
/// '<path>'`) locally, in a clean environment, as sshd would on the host.
fn fake_ssh(t: &TempDir) -> PathBuf {
    let path = t.path().join("fake-ssh");
    let git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let git_dir = Path::new(String::from_utf8(git.stdout).unwrap().trim())
        .parent()
        .unwrap()
        .to_owned();
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nfor last; do :; done\n\
             cmd=$(printf '%s' \"$last\" | sed 's/^git-receive-pack/git receive-pack/')\n\
             exec env -i PATH='{}:/usr/bin:/bin' GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null sh -c \"$cmd\"\n",
            git_dir.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// A destination bare repository in the fixture, and its ssh URL.
fn destination(t: &TempDir) -> (PathBuf, GitDestination) {
    let dest = t.path().join("dest.git");
    fs::create_dir(&dest).unwrap();
    git(&dest, &["init", "-q", "--bare"]);
    let dest = fs::canonicalize(dest).unwrap();
    let d = GitDestination {
        url: format!("ssh://git@host.invalid{}", dest.display()),
        host_key: KEY.to_owned(),
        identity: PathBuf::from("/nonexistent/id_ed25519"),
    };
    (dest, d)
}

fn runner(t: &TempDir, d: GitDestination) -> BoundPush {
    BoundPush::with_ssh(d, &t.path().join("state"), fake_ssh(t)).unwrap()
}

#[test]
fn a_push_lands_on_the_destination_and_leaves_nothing_behind() {
    let t = TempDir::new();
    let (common, sha) = worker(&t);
    let checkout = common.parent().unwrap().to_owned();
    let (dest, d) = destination(&t);
    let r = runner(&t, d);
    r.push(&checkout, "agent/a", &sha).unwrap();
    assert_eq!(git(&dest, &["rev-parse", "refs/heads/agent/a"]), sha);
    // The push's folder is gone.
    let root = push_root(&t.path().join("state"));
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
}

#[test]
fn the_workers_config_never_steers_the_push() {
    // Terra 2 #6271: a URL rewrite or hooks in the worker's own config
    // don't apply; no git runs there but mdkg-git's worktree check.
    let t = TempDir::new();
    let (common, sha) = worker(&t);
    let checkout = common.parent().unwrap().to_owned();
    let mut cfg = fs::read_to_string(common.join("config")).unwrap();
    cfg.push_str("[url \"ssh://evil.invalid/\"]\n\tpushInsteadOf = ssh://\n\tinsteadOf = ssh://\n");
    cfg.push_str(
        "[core]\n\thooksPath = /tmp/nope\n[remote \"origin\"]\n\turl = ssh://evil.invalid/x\n",
    );
    fs::write(common.join("config"), cfg).unwrap();
    let (dest, d) = destination(&t);
    runner(&t, d).push(&checkout, "agent/a", &sha).unwrap();
    assert_eq!(git(&dest, &["rev-parse", "refs/heads/agent/a"]), sha);
}

#[test]
fn a_refusal_by_the_receiving_side_is_push_refused() {
    let t = TempDir::new();
    let (common, sha) = worker(&t);
    let checkout = common.parent().unwrap().to_owned();
    let (dest, d) = destination(&t);
    let hook = dest.join("hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\necho no >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let err = runner(&t, d).push(&checkout, "agent/a", &sha).unwrap_err();
    assert_eq!(err.code, Code::RunPushRefused, "{err}");
}

#[test]
fn a_rewritten_branch_is_refused_never_forced() {
    let t = TempDir::new();
    let (common, sha) = worker(&t);
    let checkout = common.parent().unwrap().to_owned();
    let (dest, d) = destination(&t);
    let r = runner(&t, d);
    r.push(&checkout, "agent/a", &sha).unwrap();
    // A commit that doesn't descend from the pushed one.
    let w = checkout.as_path();
    git(w, &["checkout", "-q", "--orphan", "other"]);
    git(
        w,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "o",
        ],
    );
    let other = git(w, &["rev-parse", "HEAD"]);
    let err = r.push(&checkout, "agent/a", &other).unwrap_err();
    assert_eq!(err.code, Code::RunPushRefused, "{err}");
    assert_eq!(git(&dest, &["rev-parse", "refs/heads/agent/a"]), sha);
}

#[test]
fn a_commit_the_worker_lacks_or_a_bad_name_pushes_nothing() {
    let t = TempDir::new();
    let (common, _) = worker(&t);
    let checkout = common.parent().unwrap().to_owned();
    let (dest, d) = destination(&t);
    let r = runner(&t, d);
    let err = r.push(&checkout, "agent/a", &"b".repeat(40)).unwrap_err();
    assert_eq!(err.code, Code::CoordCommitUnknown, "{err}");
    for (branch, commit) in [("agent/a", "HEAD"), ("-x", "a"), ("a..b", "a")] {
        let commit = if commit == "a" {
            "a".repeat(40)
        } else {
            commit.to_owned()
        };
        let err = r.push(&checkout, branch, &commit).unwrap_err();
        assert_eq!(err.code, Code::UsageInvalidArguments, "{branch} {commit}");
    }
    assert_eq!(git(&dest, &["for-each-ref"]), "");
    // A destination outside the rules is refused before any push.
    let bad = GitDestination {
        url: "ext::sh%20-c%20x".to_owned(),
        host_key: KEY.to_owned(),
        identity: PathBuf::from("/k"),
    };
    assert!(BoundPush::with_ssh(bad, t.path(), fake_ssh(&t)).is_err());
}

#[test]
fn a_push_that_fails_short_of_the_receiving_side_says_what_to_check() {
    // Terra 2 #10767: an unreachable host, a wrong host key, or an
    // identity needing an agent or a passphrase all end here.
    let t = TempDir::new();
    let (common, sha) = worker(&t);
    let checkout = common.parent().unwrap().to_owned();
    let (_, d) = destination(&t);
    let failing = t.path().join("failing-ssh");
    fs::write(
        &failing,
        "#!/bin/sh\necho 'Permission denied (publickey).' >&2\nexit 255\n",
    )
    .unwrap();
    fs::set_permissions(&failing, fs::Permissions::from_mode(0o700)).unwrap();
    let r = BoundPush::with_ssh(d, &t.path().join("state"), failing).unwrap();
    let err = r.push(&checkout, "agent/a", &sha).unwrap_err();
    assert_eq!(err.code, Code::IoGitFailed, "{err}");
    let hint = err.hint.as_deref().unwrap();
    assert!(
        hint.contains("ssh-agent") && hint.contains("passphrase"),
        "{hint}"
    );
    // No remote text in the message.
    assert!(
        !err.message.contains("Permission denied"),
        "{}",
        err.message
    );
}
