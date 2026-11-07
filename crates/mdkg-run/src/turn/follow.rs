//! Following `mainRef` (edd-01a18867, "Following mainRef"; G15): before a
//! worker's turn, under its turn lease, the launcher tells it how far its
//! branch is behind the code base, and `agents status` shows the same
//! counts. Read-only: nothing here fetches, pushes, merges or moves any
//! ref. The opt-in merge (`follow: "merge"`) follows by notes and says so.

use super::{Pinned, full_ref, git_pinned, short_ref};

/// How the launcher follows `mainRef` for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Follow {
    /// `mainRef` as configured (or `$MDKG_MAIN_REF`): a full ref or a
    /// bare name, read as the base resolver reads it.
    pub main_ref: String,
    /// `follow: "merge"` in the agent's committed definition: followed by
    /// notes in this release, with a line saying so.
    pub merge: bool,
}

/// The notes for a worker's prompt: how far its branch is behind
/// `mainRef`, and the command that merges it. Nothing when it's level, or
/// mainRef isn't here.
pub(crate) fn follow(pin: &Pinned, follow: &Follow) -> Vec<String> {
    let Some((main, ahead, behind)) = lag(pin, &follow.main_ref) else {
        return Vec::new();
    };
    if behind == 0 {
        return Vec::new();
    }
    let name = short_ref(&main);
    // The full ref in the command (a local branch or tag spelled like the
    // short name would win over it in `git merge`), quoted as one shell
    // word: a ref name may hold `$` or a backquote.
    let mut notes = vec![format!(
        "your branch is {behind} commit{} behind {name} and {ahead} ahead; merge it when it \
         suits the work (`git merge {}`)",
        plural(behind),
        shell_word(&main)
    )];
    if follow.merge {
        notes.push(
            "your definition asks for `follow: \"merge\"`; the launcher's automatic merge \
             is not available in this release, so merge it yourself"
                .into(),
        );
    }
    notes
}

/// How a worker's branch stands against `main_ref`, for `agents status`:
/// the full ref, and the commits it's ahead and behind. `None` when
/// either can't be read.
pub fn lag(pin: &Pinned, main_ref: &str) -> Option<(String, u64, u64)> {
    let main = full_ref(&pin.git_dir, main_ref).ok().flatten()?;
    let (ahead, behind) = ahead_behind(pin, &main)?;
    Some((main, ahead, behind))
}

/// `s` as one word for a POSIX shell.
fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Commits on HEAD that `main` lacks, and on `main` that HEAD lacks.
fn ahead_behind(pin: &Pinned, main: &str) -> Option<(u64, u64)> {
    let range = format!("HEAD...{main}");
    let out = git_pinned(
        pin,
        &pin.top,
        &[
            "rev-list",
            "--left-right",
            "--count",
            "--end-of-options",
            &range,
        ],
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut counts = text.split_whitespace().map(str::parse::<u64>);
    match (counts.next(), counts.next()) {
        (Some(Ok(a)), Some(Ok(b))) => Some((a, b)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A repository on `main` with a worker's worktree on `agent/w`,
    /// removed on drop.
    struct Fixture {
        dir: PathBuf,
        repo: PathBuf,
        work: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("mdkg-follow-{}", uuid::Uuid::now_v7()));
            let repo = dir.join("repo");
            fs::create_dir_all(&repo).expect("mkdir");
            let f = Self {
                work: dir.join("w"),
                dir,
                repo,
            };
            f.git(&f.repo, &["init", "--quiet", "-b", "main"]);
            f.commit(&f.repo, "a");
            let work = f.work.display().to_string();
            f.git(
                &f.repo,
                &["worktree", "add", "--quiet", "-b", "agent/w", &work],
            );
            f
        }

        fn git(&self, at: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(at)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn commit(&self, at: &Path, file: &str) -> String {
            fs::write(at.join(file), file).expect("w");
            self.git(at, &["add", file]);
            self.git(at, &["commit", "--quiet", "-m", file]);
            self.git(at, &["rev-parse", "HEAD"])
        }

        fn pin(&self) -> Pinned {
            let common = fs::canonicalize(self.repo.join(".git")).expect("common");
            super::super::find_pin(&common, &self.work).expect("pin")
        }

        fn head(&self) -> String {
            self.git(&self.work, &["rev-parse", "HEAD"])
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn follow_main(merge: bool) -> Follow {
        Follow {
            main_ref: "main".into(),
            merge,
        }
    }

    #[test]
    fn a_branch_behind_main_gets_a_note_and_nothing_moves() {
        let f = Fixture::new();
        // Nothing to say while level.
        assert!(follow(&f.pin(), &follow_main(false)).is_empty());
        f.commit(&f.repo, "b");
        f.commit(&f.repo, "c");
        f.commit(&f.work, "mine");
        let head = f.head();
        let notes = follow(&f.pin(), &follow_main(false));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with("your branch is 2 commits behind main"),
            "{notes:?}"
        );
        assert!(notes[0].contains("1 ahead"));
        assert!(notes[0].contains("`git merge 'refs/heads/main'`"));
        assert_eq!(f.head(), head);
        // A mainRef that isn't here says nothing.
        let missing = Follow {
            main_ref: "origin/main".into(),
            merge: false,
        };
        assert!(follow(&f.pin(), &missing).is_empty());
    }

    #[test]
    fn follow_merge_is_notes_in_this_release() {
        let f = Fixture::new();
        f.commit(&f.repo, "b");
        let head = f.head();
        let notes = follow(&f.pin(), &follow_main(true));
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes[1].contains("is not available in this release"),
            "{notes:?}"
        );
        assert_eq!(f.head(), head, "nothing merged");
    }

    #[test]
    fn a_ref_is_quoted_as_one_shell_word() {
        assert_eq!(shell_word("refs/heads/main"), "'refs/heads/main'");
        assert_eq!(
            shell_word("refs/heads/a$(x)'b"),
            r#"'refs/heads/a$(x)'\''b'"#
        );
    }

    #[test]
    fn lag_reads_ahead_and_behind() {
        let f = Fixture::new();
        f.commit(&f.repo, "b");
        f.commit(&f.work, "mine");
        assert_eq!(
            lag(&f.pin(), "main"),
            Some(("refs/heads/main".into(), 1, 1))
        );
        assert_eq!(lag(&f.pin(), "origin/main"), None);
    }
}
