//! Local invalidation only: a changed file wakes dispatch, whose existing
//! admission checks remain the authority. No coordinator or model polling.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::{hooks, state};

type Stamp = Option<(u64, u64, u32, u64, i64, i64, i64, i64)>;

fn stamp(path: &Path) -> io::Result<Stamp> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some((
            m.dev(),
            m.ino(),
            m.mode(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    files: Vec<(PathBuf, Stamp)>,
    // An idle dispatch rewrites the route with identical bytes. Comparing
    // its contents avoids treating our own write as another wake.
    route: Option<Vec<u8>>,
    threads_dir: Option<(u64, u64, u32)>,
}

type ThreadCache = BTreeMap<PathBuf, (Stamp, bool)>;

impl Snapshot {
    fn read(common: &Path, run: &Path, actor: &str, cache: &mut ThreadCache) -> io::Result<Self> {
        let receiver = hooks::dir(common).join("receiver");
        let name = state::file_name(actor);
        let path = receiver.join(format!("{name}.json"));
        let route = match stamp(&path)? {
            None => None,
            Some(_) => {
                // Do not follow links or block on a special file. This is
                // the same bounded, no-follow read as receiver admission.
                use std::io::Read;
                use std::os::unix::fs::OpenOptionsExt;
                let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
                let file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(flags.bits() as i32)
                    .open(path)?;
                if !file.metadata()?.is_file() {
                    return Err(io::Error::other("receiver route is not a regular file"));
                }
                let mut bytes = Vec::new();
                file.take((1 << 20) + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 1 << 20 {
                    return Err(io::Error::other("receiver route is too large"));
                }
                Some(bytes)
            }
        };
        let cwd = route
            .as_ref()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
            .and_then(|value| value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from));
        let mut paths = vec![
            state::state_path(run, actor),
            receiver.join(format!("{name}.pending")),
            receiver.join(format!("{name}.observed")),
        ];
        let threads = hooks::dir(common).join("threads");
        // Directory churn from unrelated actors must not wake every receiver.
        // Keep its identity, and watch only records reconcile() would consume.
        let threads_dir = match fs::symlink_metadata(&threads) {
            Ok(m) if m.is_dir() => Some((m.dev(), m.ino(), m.mode())),
            Ok(_) => {
                return Err(io::Error::other(
                    "receiver thread registry is not a directory",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let mut seen = BTreeSet::new();
        match fs::read_dir(&threads) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name();
                    if !name.to_string_lossy().starts_with("codex-")
                        || !name.to_string_lossy().ends_with(".json")
                    {
                        continue;
                    }
                    if seen.len() >= 4096 {
                        return Err(io::Error::other("receiver thread registry is too large"));
                    }
                    let path = entry.path();
                    let at = stamp(&path)?;
                    let relevant = match cache.get(&path).filter(|(kept, _)| kept == &at) {
                        Some((_, relevant)) => *relevant,
                        None => {
                            // A writer may be replacing this record. Preserve
                            // its prior scope until actor/cwd can be read;
                            // dispatch validates the full registry under lock.
                            let value = super::read_json(&path).ok().flatten();
                            let relevant = match value.as_ref().and_then(|v| {
                                Some((v.get("actor")?.as_str()?, v.get("cwd")?.as_str()?))
                            }) {
                                Some((owner, thread_cwd)) => {
                                    owner == actor
                                        || cwd.as_ref().is_some_and(|cwd| {
                                            Path::new(thread_cwd).starts_with(cwd)
                                        })
                                }
                                None => cache.get(&path).is_some_and(|(_, relevant)| *relevant),
                            };
                            cache.insert(path.clone(), (at, relevant));
                            relevant
                        }
                    };
                    seen.insert(path.clone());
                    if relevant {
                        paths.push(path);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        cache.retain(|path, _| seen.contains(path));
        paths.sort();
        let files = paths
            .into_iter()
            .map(|p| stamp(&p).map(|s| (p, s)))
            .collect::<io::Result<_>>()?;
        Ok(Self {
            files,
            route,
            threads_dir,
        })
    }
}

pub(super) struct Wake<'a> {
    common: &'a Path,
    run: &'a Path,
    actor: &'a str,
    before: Snapshot,
    threads: RefCell<ThreadCache>,
}

impl<'a> Wake<'a> {
    pub(super) fn new(common: &'a Path, run: &'a Path, actor: &'a str) -> io::Result<Self> {
        let mut threads = ThreadCache::new();
        let before = Snapshot::read(common, run, actor, &mut threads)?;
        Ok(Self {
            common,
            run,
            actor,
            before,
            threads: RefCell::new(threads),
        })
    }

    pub(super) fn changed(&self) -> bool {
        match Snapshot::read(
            self.common,
            self.run,
            self.actor,
            &mut self.threads.borrow_mut(),
        ) {
            Ok(now) => now != self.before,
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CliMail, Mail};

    #[test]
    fn unrelated_thread_activity_does_not_invalidate_an_idle_receiver() {
        let root =
            std::env::temp_dir().join(format!("mdkg-receiver-unrelated-{}", uuid::Uuid::now_v7()));
        let common = root.join(".git");
        let run = common.join("mdkg/run");
        let threads = hooks::dir(&common).join("threads");
        let receiver = hooks::dir(&common).join("receiver");
        fs::create_dir_all(&run).unwrap();
        fs::create_dir_all(&threads).unwrap();
        fs::create_dir_all(&receiver).unwrap();
        fs::write(
            receiver.join("idle.json"),
            serde_json::json!({"cwd": root.join("mine")}).to_string(),
        )
        .unwrap();
        let wake = Wake::new(&common, &run, "idle").unwrap();
        let thread = threads.join("codex-fixture.json");
        for n in 0..5 {
            // Neither a partial update nor a new record with no readable
            // identity should wake all receivers sharing this registry.
            fs::write(&thread, "{").unwrap();
            assert!(!wake.changed());
            fs::write(
                &thread,
                serde_json::json!({"actor":"other", "cwd":root.join("elsewhere"), "at":n})
                    .to_string(),
            )
            .unwrap();
            assert!(!wake.changed());
        }
        fs::remove_file(&thread).unwrap();
        assert!(!wake.changed());
        // A different actor in our selected worktree can invalidate routing.
        fs::write(
            &thread,
            serde_json::json!({"actor":"other", "cwd":root.join("mine/child")}).to_string(),
        )
        .unwrap();
        assert!(wake.changed());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_change_before_waiting_does_not_start_a_child() {
        let root = std::env::temp_dir().join(format!("mdkg-receiver-gap-{}", uuid::Uuid::now_v7()));
        let common = root.join(".git");
        let run = common.join("mdkg/run");
        fs::create_dir_all(&run).unwrap();
        let wake = Wake::new(&common, &run, "idle").unwrap();
        // Simulate state arriving between the dispatch snapshot and wait.
        fs::write(state::state_path(&run, "idle"), "{}").unwrap();
        let mut mail = CliMail::new(root.join("must-not-start"), root.clone(), "launcher".into());
        assert!(
            mail.wait_headers_waking("idle", None, 600, &|| wake.changed())
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
