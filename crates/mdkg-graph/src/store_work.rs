//! Where a graph keeps its work-type nodes under `work.committed: none`
//! (profile C): beside the coordinator store, in the git
//! common folder, which git never tracks and every worktree shares.
//!
//! Found from the files git itself writes (`.git` as a folder, or a
//! `gitdir:` file and its `commondir`), with no git process, so loading a
//! graph stays a pure read.

use std::path::{Path, PathBuf};

/// The work types profile C keeps out of git: task, bug, spike,
/// test and checkpoint definitions.
pub const STORE_WORK_TYPES: &[&str] = &["task", "bug", "spike", "test", "checkpoint"];

/// The folder for the graph at `root`: `None` outside a git repository,
/// `<common>/mdkg/work/root` for a graph at the top of its worktree, else
/// `<common>/mdkg/work/at-<path>` with `/` written `%2f` (and `%` as
/// `%25`), so each graph in a repository has its own. A `.git` that is
/// there but can't be read, or isn't what git writes, is an error, never
/// "not a repository".
///
/// In ref mode a graph's root is the clone's graph cache,
/// `<common>/mdkg/graph/cache[/<prefix>]`, which mirrors the repository's
/// layout and is inside the git folder, where no checkout is found. Its
/// work stays where checkout mode keeps it, `under(common, prefix)`, so
/// local work survives a move to the graph ref (edd-01a18867, Config,
/// "Local work stays local"; R1, C25).
pub fn work_dir(root: &Path) -> Result<Option<PathBuf>, String> {
    let found = match mdkg_format::settings::git::discover(root) {
        Ok(found) => found,
        Err(e) => return in_cache(root).map(Some).ok_or(e),
    };
    Ok(found.map(|repo| under(&repo.common, &slashed(&repo.prefix))))
}

fn slashed(prefix: &Path) -> String {
    prefix
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The work folder for a root inside a git common folder's graph cache:
/// only a root `discover` refused, whose ancestor `<C>/mdkg/graph/cache`
/// has a `C` that is a git folder (`HEAD`, `objects`, `refs`).
fn in_cache(root: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    let cache = root.ancestors().find(|d| {
        d.ends_with("mdkg/graph/cache")
            && d.ancestors().nth(3).is_some_and(|c| {
                c.join("HEAD").is_file() && c.join("objects").is_dir() && c.join("refs").is_dir()
            })
    })?;
    let common = cache.ancestors().nth(3)?;
    let prefix = root.strip_prefix(cache).ok()?;
    Some(under(common, &slashed(prefix)))
}

/// Whether a node's path (as the index gives it) is in the store's work
/// folder: absolute (outside the graph root) or under `.git/`. No node in
/// the committed planes has either.
pub fn is_store_path(path: &str) -> bool {
    path.starts_with('/') || path.starts_with(".git/") || Path::new(path).is_absolute()
}

/// The folder under a common folder for a graph at `prefix`.
pub fn under(common: &Path, prefix: &str) -> PathBuf {
    let key = prefix.trim_matches('/');
    let key = if key.is_empty() {
        "root".to_string()
    } else {
        format!("at-{}", key.replace('%', "%25").replace('/', "%2f"))
    };
    common.join("mdkg").join("work").join(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(common: &Path) {
        for name in ["objects", "refs"] {
            std::fs::create_dir_all(common.join(name)).unwrap();
        }
        std::fs::write(common.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(common.join("config"), "[core]\n bare = false\n").unwrap();
    }

    #[test]
    fn each_graph_gets_its_own_folder() {
        let common = Path::new("/r/.git");
        assert_eq!(under(common, ""), Path::new("/r/.git/mdkg/work/root"));
        assert_eq!(
            under(common, "sub/dir"),
            Path::new("/r/.git/mdkg/work/at-sub%2fdir")
        );
        assert_ne!(under(common, "a%2fb"), under(common, "a/b"));
    }

    #[test]
    fn found_through_a_worktree_s_gitdir_file() {
        let dir = std::env::temp_dir().join(format!("mdkg-store-work-{}", std::process::id()));
        let main = dir.join("main");
        let common = main.join(".git");
        repository(&common);
        std::fs::create_dir_all(common.join("worktrees/wt")).unwrap();
        std::fs::write(common.join("worktrees/wt/commondir"), "../..\n").unwrap();
        std::fs::write(common.join("worktrees/wt/HEAD"), "ref: refs/heads/wt\n").unwrap();
        let wt = dir.join("wt");
        std::fs::create_dir_all(wt.join("sub")).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", common.join("worktrees/wt").display()),
        )
        .unwrap();
        let common = std::fs::canonicalize(&common).unwrap();
        assert_eq!(work_dir(&main), Ok(Some(under(&common, ""))));
        assert_eq!(work_dir(&wt), Ok(Some(under(&common, ""))));
        assert_eq!(work_dir(&wt.join("sub")), Ok(Some(under(&common, "sub"))));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ref mode: a graph root in the graph cache keeps checkout mode's
    /// work folder, and a look-alike outside a git folder isn't one.
    #[test]
    fn a_graph_cache_root_keeps_the_checkout_s_work_folder() {
        let dir = std::env::temp_dir().join(format!("mdkg-store-work-c-{}", std::process::id()));
        let common = dir.join("r/.git");
        repository(&common);
        let cache = common.join("mdkg/graph/cache");
        std::fs::create_dir_all(cache.join("sub/dir")).unwrap();
        let common = std::fs::canonicalize(&common).unwrap();
        assert_eq!(work_dir(&cache), Ok(Some(under(&common, ""))));
        assert_eq!(
            work_dir(&cache.join("sub/dir")),
            Ok(Some(under(&common, "sub/dir")))
        );
        // The same names in a folder that isn't a git folder.
        let fake = dir.join("plain/mdkg/graph/cache");
        std::fs::create_dir_all(&fake).unwrap();
        assert_eq!(work_dir(&fake), Ok(None));
        // Elsewhere inside the git folder is still refused.
        assert!(work_dir(&common.join("mdkg")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review of 541f23b: git's exact form, whitespace in a path kept, and
    /// a FIFO or a malformed file is an error, never a hang or "no repo".
    #[test]
    fn gitdir_files_are_read_exactly_and_safely() {
        let dir = std::env::temp_dir().join(format!("mdkg-store-work-x-{}", std::process::id()));
        let meta = dir.join("meta ");
        repository(&meta);
        let wt = dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", meta.display())).unwrap();
        let meta = std::fs::canonicalize(&meta).unwrap();
        assert_eq!(
            work_dir(&wt),
            Ok(Some(under(&meta, ""))),
            "the trailing space kept"
        );

        std::fs::write(wt.join(".git"), "gitdir:nope\n").unwrap();
        assert!(work_dir(&wt).is_err(), "not git's form");

        std::fs::remove_file(wt.join(".git")).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(wt.join(".git"))
            .status()
            .unwrap();
        assert!(made.success());
        assert!(work_dir(&wt).is_err(), "a FIFO is refused at once");
        std::fs::remove_dir_all(&dir).ok();
    }
}
