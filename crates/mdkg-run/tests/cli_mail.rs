//! [`CliMail`] against a stub of the M1 CLI (`tests/fixtures/fake-mdkg.sh`):
//! the command lines it builds, the envelopes it parses and the exit codes
//! it maps.

use std::fs;
use std::path::{Path, PathBuf};

use mdkg_run::mail::Import;
use mdkg_run::{CliMail, ErrorKind, Mail};

fn setup(name: &str) -> (PathBuf, CliMail) {
    let root = std::env::temp_dir().join(format!("mdkg-climail-{name}-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&root).expect("root");
    let mail = CliMail {
        program: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-mdkg.sh"),
        root: root.clone(),
        launcher: "launcher".into(),
        holder: "launcher@run-1".into(),
        trusted_common: Some(root.join("common")),
        trusted_worktree: Some(root.join("top")),
    };
    (root, mail)
}

fn log(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join("fake.log"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn every_call_names_the_trusted_common_dir_and_worktree() {
    let (root, mut mail) = setup("trusted");
    mail.inbox("agent", 5).expect("inbox");
    let seen = fs::read_to_string(root.join("trusted.log")).expect("log");
    assert_eq!(seen.trim(), root.join("common").display().to_string());
    let seen = fs::read_to_string(root.join("trusted-worktree.log")).expect("log");
    assert_eq!(seen.trim(), root.join("top").display().to_string());
}

#[test]
fn inbox_reads_as_the_agent_and_parses_the_contract() {
    let (root, mut mail) = setup("inbox");
    let inbox = mail.inbox("lane", 20).expect("inbox");
    assert_eq!(inbox.cursor, Some(5));
    assert_eq!(inbox.messages[0].message_id, "m1");
    assert_eq!(
        log(&root),
        [format!(
            "--root {} --as=lane msg inbox --limit 20 --json",
            root.display()
        )]
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn import_uses_equals_flags_and_puts_the_file_after_a_separator() {
    let (root, mut mail) = setup("import");
    let id = mail
        .import(&Import {
            kind: "findings",
            from: "codex:pr1",
            reply_to: "-looks-like-a-flag",
            to: "orchestrator",
            room: None,
            context: Some("room:review"),
            file: Path::new("/tmp/reply"),
        })
        .expect("import");
    assert_eq!(id, "import:abc");
    assert_eq!(
        log(&root),
        [format!(
            "--root {} --as=launcher msg import --kind=findings --from=codex:pr1 \
             --reply-to=-looks-like-a-flag --to=orchestrator --context=room:review --json -- /tmp/reply",
            root.display()
        )]
    );
    let e = mail
        .import(&Import {
            kind: "note",
            from: "a",
            reply_to: "m1",
            to: "x,role:orchestrator",
            room: None,
            context: None,
            file: Path::new("/tmp/reply"),
        })
        .expect_err("comma");
    assert_eq!(e.code, "run.mail_failed");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_room_reply_goes_into_the_room_and_the_lease_is_held_per_process() {
    let (root, mut mail) = setup("room");
    mail.import(&Import {
        kind: "note",
        from: "lane",
        reply_to: "m1",
        to: "b",
        room: Some("review"),
        context: None,
        file: Path::new("/tmp/reply"),
    })
    .expect("import");
    mail.claim("run:lane").expect("claim");
    let log = log(&root);
    assert!(log[0].contains(" --room=review ") && !log[0].contains("--to="));
    assert!(log[1].contains("--as=launcher@run-1 claim run:lane"));
    let other = CliMail::new(PathBuf::from("x"), root.clone(), "launcher".into());
    let again = CliMail::new(PathBuf::from("x"), root.clone(), "launcher".into());
    assert!(other.holder.starts_with("launcher@run-") && other.holder != again.holder);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn ack_ids_come_after_a_separator() {
    let (root, mut mail) = setup("ack");
    mail.ack("lane", &["-m1".into()], "replied x").expect("ack");
    assert_eq!(
        log(&root),
        [format!(
            "--root {} --as=lane msg ack --outcome=replied x --json -- -m1",
            root.display()
        )]
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_wait_timeout_is_none_and_a_held_claim_is_a_conflict() {
    let (root, mut mail) = setup("codes");
    assert!(mail.wait("lane", Some(3), 1).expect("wait").is_some());
    assert!(log(&root)[0].contains("msg wait --timeout 1 --after 3"));
    fs::write(root.join("fake-wait"), "").expect("w");
    assert_eq!(mail.wait("lane", None, 1).expect("timeout"), None);
    fs::write(root.join("fake-claim"), "").expect("w");
    let e = mail.claim("run:lane").expect_err("held");
    assert_eq!(
        (e.kind, e.code.as_str()),
        (ErrorKind::Conflict, "coord.lease_held")
    );
    assert_eq!(e.hint.as_deref(), Some("pick other work"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn output_that_isnt_the_contract_is_a_mail_failure() {
    let (root, mut mail) = setup("garbage");
    mail.program = PathBuf::from("/bin/echo");
    let e = mail.release("run:lane").expect_err("not json");
    assert_eq!(e.code, "run.mail_failed");
    let _ = fs::remove_dir_all(root);
}
