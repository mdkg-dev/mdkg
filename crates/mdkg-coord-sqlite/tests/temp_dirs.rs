//! Fixture allocation must preserve existing candidates and isolate callers.

#[path = "common/temp.rs"]
mod temp;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

#[test]
fn existing_directory_and_file_are_preserved() {
    let parent = Scratch(temp::dir("mdkg-temp-collision"));
    let first = temp::create_in(&parent.0, "candidate", &AtomicU64::new(0)).unwrap();
    fs::write(first.join("sentinel"), "owned by another fixture").unwrap();
    let second = parent.0.join(format!("candidate-{}-1", std::process::id()));
    fs::write(&second, "also occupied").unwrap();
    let reserved = temp::create_in(&parent.0, "candidate", &AtomicU64::new(0)).unwrap();
    assert_ne!(reserved, first);
    assert_ne!(reserved, second);
    assert!(reserved.is_dir());
    assert_eq!(
        fs::read_to_string(first.join("sentinel")).unwrap(),
        "owned by another fixture"
    );
    assert_eq!(fs::read_to_string(second).unwrap(), "also occupied");
}

#[test]
fn parallel_callers_own_distinct_directories() {
    let dirs: Vec<_> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..16)
            .map(|_| {
                scope.spawn(|| {
                    (0..16)
                        .map(|_| Scratch(temp::dir("mdkg-temp-parallel")))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect()
    });
    let unique: HashSet<_> = dirs.iter().map(|d| &d.0).collect();
    assert_eq!(unique.len(), 256);
    assert!(dirs.iter().all(|d| d.0.is_dir()));
}

#[test]
fn non_collision_errors_are_returned() {
    let parent = Scratch(temp::dir("mdkg-temp-error"));
    let error =
        temp::create_in(&parent.0.join("missing"), "candidate", &AtomicU64::new(0)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
