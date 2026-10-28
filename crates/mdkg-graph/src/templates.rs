//! Template schemas: which keys each node type may carry.
//!
//! Every `.md` file under `<templates_root>/<templates_set>/` contributes the
//! keys of its frontmatter to the schema of the type it declares. Types with
//! no local template fall back to the bundled 0.6.0 templates.

use std::collections::HashMap;
use std::path::Path;

use mdkg_format::frontmatter;
use mdkg_format::node::{ALLOWED_TYPES, TemplateSchema};

use crate::bundled::BUNDLED_DEFAULT;
use crate::config::Config;
use mdkg_format::Code;

use crate::error::GraphError;
use crate::fs_walk;
use crate::index::relative_path;

fn merge(
    schemas: &mut HashMap<String, TemplateSchema>,
    content: &str,
    path: &str,
) -> Result<(), GraphError> {
    let parsed = frontmatter::parse(content, path)
        .map_err(|e| GraphError::from_format(&e, Path::new("")))?;
    let Some(node_type) = parsed.frontmatter.get("type").and_then(|v| v.as_str()) else {
        return Err(GraphError::failure(
            Code::TemplateInvalid,
            format!("template missing type in {path}"),
        ));
    };
    if node_type != node_type.to_lowercase() {
        return Err(GraphError::failure(
            Code::TemplateInvalid,
            format!("template type must be lowercase in {path}"),
        ));
    }
    let incoming = TemplateSchema::from_frontmatter(node_type, &parsed.frontmatter);
    match schemas.get_mut(node_type) {
        None => {
            schemas.insert(node_type.to_string(), incoming);
        }
        Some(existing) => {
            for key in incoming.keys() {
                if let (Some(a), Some(b)) = (existing.kind(key), incoming.kind(key))
                    && a != b
                {
                    return Err(GraphError::failure(
                        Code::TemplateInvalid,
                        format!(
                            "template schema mismatch for {node_type}.{key}: {} vs {} ({path})",
                            kind_name(a),
                            kind_name(b)
                        ),
                    ));
                }
            }
            *existing = existing.merged_with(&incoming);
        }
    }
    Ok(())
}

/// Reads a template, `None` when there's none. A template that leads out
/// of the repository through a link is refused (audit, 2026-10-22): the
/// config checks the folder at load, and a file or folder can be linked
/// since. It's read through folder handles from the repository root
/// ([`open_inside`]), so a link swapped in can't win (reviews of c99eb3b
/// and 13b3e6e).
fn read_template(root: &Path, path: &Path) -> Result<Option<String>, GraphError> {
    use std::io::Read as _;
    let failed = |e: std::io::Error| {
        GraphError::failure(
            Code::IoReadFailed,
            format!("failed to read {}: {e}", path.display()),
        )
    };
    let rel = path.strip_prefix(root).map_err(|_| {
        failed(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not in the repository",
        ))
    })?;
    let mut f = match open_inside(root, rel).map_err(failed)? {
        Opened::File(f) => f,
        Opened::Missing => return Ok(None),
        Opened::Outside => {
            return Err(GraphError::failure(
                Code::TemplateInvalid,
                format!(
                    "template {} leads outside the repository through a link",
                    relative_path(root, path)
                ),
            ));
        }
    };
    let mut text = String::new();
    f.read_to_string(&mut text).map_err(failed)?;
    Ok(Some(text))
}

/// What [`open_inside`] found.
#[derive(Debug)]
enum Opened {
    File(std::fs::File),
    Missing,
    /// A link on the way leads out of the repository.
    Outside,
}

/// Links followed in one lookup at most, as the system's own limit.
const MAX_HOPS: usize = 40;

/// Opens the file `rel` below `root` through folder handles opened from
/// the repository root's, one name at a time, none through a link: a link
/// on the way is resolved, must stay in the repository, and the walk starts
/// again from the root along where it leads. So every handle is reached
/// from the root by real folders, and no second lookup by path decides
/// anything a swap could win (review of 13b3e6e). The file is opened
/// without blocking and must be a regular file (a FIFO can't hang a read).
fn open_inside(root: &Path, rel: &Path) -> std::io::Result<Opened> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStrExt as _;
    let dir = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let canon = std::fs::canonicalize(root)?;
    let base = rustix::fs::open(
        &canon,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut todo: Vec<OsString> = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(n) => todo.push(n.to_os_string()),
            std::path::Component::CurDir => {}
            _ => return Ok(Opened::Outside),
        }
    }
    let mut hops = 0;
    'walk: loop {
        let mut cur = rustix::fs::openat(&base, ".", dir, Mode::empty())?;
        let mut real = std::path::PathBuf::new();
        for (i, name) in todo.iter().enumerate() {
            let last = i + 1 == todo.len();
            let flags = if last {
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK
            } else {
                dir
            };
            match rustix::fs::openat(&cur, name.as_os_str(), flags, Mode::empty()) {
                Ok(fd) if last => {
                    if !FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode).is_file() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "it isn't a regular file",
                        ));
                    }
                    let flags = rustix::fs::fcntl_getfl(&fd)?;
                    rustix::fs::fcntl_setfl(&fd, flags - OFlags::NONBLOCK)?;
                    return Ok(Opened::File(std::fs::File::from(fd)));
                }
                Ok(fd) => {
                    cur = fd;
                    real.push(name);
                }
                Err(Errno::NOENT) => return Ok(Opened::Missing),
                Err(e) => {
                    let link =
                        rustix::fs::statat(&cur, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)
                            .is_ok_and(|s| FileType::from_raw_mode(s.st_mode).is_symlink());
                    if !link {
                        return Err(e.into());
                    }
                    hops += 1;
                    if hops > MAX_HOPS {
                        return Err(Errno::LOOP.into());
                    }
                    let target = rustix::fs::readlinkat(&cur, name.as_os_str(), Vec::new())?;
                    let target = Path::new(std::ffi::OsStr::from_bytes(target.as_bytes()));
                    let Ok(resolved) = std::fs::canonicalize(canon.join(&real).join(target)) else {
                        return Ok(Opened::Missing);
                    };
                    let Ok(inside) = resolved.strip_prefix(&canon) else {
                        return Ok(Opened::Outside);
                    };
                    let mut next: Vec<OsString> = inside
                        .components()
                        .map(|c| c.as_os_str().to_os_string())
                        .collect();
                    next.extend(todo[i + 1..].iter().cloned());
                    todo = next;
                    continue 'walk;
                }
            }
        }
        // The root itself isn't a template.
        return Ok(Opened::Missing);
    }
}

fn kind_name(kind: mdkg_format::node::KeyKind) -> &'static str {
    match kind {
        mdkg_format::node::KeyKind::Scalar => "scalar",
        mdkg_format::node::KeyKind::List => "list",
        mdkg_format::node::KeyKind::Boolean => "boolean",
    }
}

fn bundled_file_for_type(node_type: &str) -> &str {
    if node_type == "checkpoint" {
        "chk"
    } else {
        node_type
    }
}

/// The template text for creating a `node_type` node: the local template,
/// or the bundled one (the flag is true for bundled).
pub fn template_text(
    root: &Path,
    config: &Config,
    node_type: &str,
) -> Result<(String, bool), GraphError> {
    let name = bundled_file_for_type(node_type);
    let local = root
        .join(&config.templates_root)
        .join(&config.templates_set)
        .join(format!("{name}.md"));
    match read_template(root, &local) {
        Ok(Some(text)) => return Ok((text, false)),
        Err(e) if e.diagnostic.code == Code::TemplateInvalid => return Err(e),
        // Unreadable, as before: the bundled one.
        _ => {}
    }
    BUNDLED_DEFAULT
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, content)| ((*content).to_string(), true))
        .ok_or_else(|| {
            GraphError::failure(
                Code::TemplateMissing,
                format!("no template for type {node_type}"),
            )
        })
}

/// Template schemas plus the types that fell back to bundled templates.
#[derive(Debug, Clone)]
pub struct Templates {
    pub schemas: HashMap<String, TemplateSchema>,
    /// In allowed-type order; `spec` (an alias) is never listed.
    pub fallback_types: Vec<String>,
}

/// Loads template schemas for every allowed type.
pub fn load(root: &Path, config: &Config) -> Result<Templates, GraphError> {
    // Format 0.7's validity is its node schema; templates only seed `new`.
    if config.format == mdkg_format::node::FormatVersion::V07 {
        return Ok(Templates {
            schemas: mdkg_format::schema07::schemas().clone(),
            fallback_types: Vec::new(),
        });
    }
    let mut schemas = HashMap::new();
    let mut fallback_types = Vec::new();
    let set_dir = root
        .join(&config.templates_root)
        .join(&config.templates_set);
    if set_dir.is_dir() {
        for file in fs_walk::markdown_files(&set_dir, config.limits.max_depth)? {
            // Gone since it was listed: as if it never was.
            let Some(content) = read_template(root, &file)? else {
                continue;
            };
            merge(&mut schemas, &content, &relative_path(root, &file))?;
        }
    }
    for node_type in ALLOWED_TYPES {
        if schemas.contains_key(*node_type) {
            continue;
        }
        // `spec` is an alias of `manifest`.
        if *node_type == "spec" {
            let base = match schemas.get("manifest") {
                Some(s) => s.clone(),
                None => bundled_schema("manifest")?,
            };
            schemas.insert("spec".to_string(), base.renamed("spec"));
            continue;
        }
        schemas.insert(node_type.to_string(), bundled_schema(node_type)?);
        fallback_types.push(node_type.to_string());
    }
    Ok(Templates {
        schemas,
        fallback_types,
    })
}

fn bundled_schema(node_type: &str) -> Result<TemplateSchema, GraphError> {
    let name = bundled_file_for_type(node_type);
    let Some((_, content)) = BUNDLED_DEFAULT.iter().find(|(n, _)| *n == name) else {
        return Err(GraphError::failure(
            Code::TemplateMissing,
            format!("bundled template fallback missing for type {node_type}"),
        ));
    };
    let mut schemas = HashMap::new();
    merge(&mut schemas, content, &format!("<bundled>/{name}.md"))?;
    schemas.remove(node_type).ok_or_else(|| {
        GraphError::failure(
            Code::TemplateMissing,
            format!("bundled template fallback type mismatch for {node_type}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_template_file_linked_out_of_the_repository_is_refused_when_read() {
        // audit, 2026-10-22: the config checks the folder, not each file.
        let dir = std::env::temp_dir().join(format!(
            "mdkg-templates-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let (repo, outside) = (dir.join("repo"), dir.join("outside"));
        let set = repo.join(".mdkg/templates/default");
        std::fs::create_dir_all(&set).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(repo.join(crate::config::CONFIG_PATH), "{}").expect("w");
        let config = crate::config::load(&repo).expect("config");
        std::fs::write(outside.join("secret.md"), "---\ntype: task\n---\n").expect("w");
        std::os::unix::fs::symlink(outside.join("secret.md"), set.join("task.md")).expect("ln");
        let e = template_text(&repo, &config, "task").expect_err("linked out");
        assert!(
            e.diagnostic
                .message
                .contains("leads outside the repository"),
            "{}",
            e.diagnostic.message
        );
        // A link that stays inside is read.
        std::fs::remove_file(set.join("task.md")).expect("rm");
        std::fs::write(repo.join("mine.md"), "---\ntype: task\n---\n").expect("w");
        std::os::unix::fs::symlink(repo.join("mine.md"), set.join("task.md")).expect("ln");
        let (text, bundled) = template_text(&repo, &config, "task").expect("inside");
        assert!(!bundled && text.contains("type: task"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_template_link_swapped_while_it_is_read_never_yields_the_outside_file() {
        // Review of c99eb3b: a check before the read lost 250 of 3000 races.
        let dir = std::env::temp_dir().join(format!(
            "mdkg-templates-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let (repo, outside) = (dir.join("repo"), dir.join("outside"));
        let set = repo.join(".mdkg/templates/default");
        std::fs::create_dir_all(&set).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(repo.join(crate::config::CONFIG_PATH), "{}").expect("w");
        let config = crate::config::load(&repo).expect("config");
        std::fs::write(repo.join("mine.md"), "---\ntype: task\n---\nINSIDE\n").expect("w");
        std::fs::write(outside.join("secret.md"), "---\ntype: task\n---\nOUTSIDE\n").expect("w");
        let task = set.join("task.md");
        std::os::unix::fs::symlink(repo.join("mine.md"), &task).expect("ln");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let (stop, set, task) = (stop.clone(), set.clone(), task.clone());
            let targets = [repo.join("mine.md"), outside.join("secret.md")];
            std::thread::spawn(move || {
                let mut i = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let tmp = set.join("swap.tmp");
                    let _ = std::fs::remove_file(&tmp);
                    if std::os::unix::fs::symlink(&targets[i % 2], &tmp).is_ok() {
                        let _ = std::fs::rename(&tmp, &task);
                    }
                    i += 1;
                }
            })
        };
        for _ in 0..1000 {
            if let Ok((text, _)) = template_text(&repo, &config, "task") {
                assert!(!text.contains("OUTSIDE"), "read the outside file");
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        swapper.join().expect("swapper");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_repository_named_through_the_data_volume_reads_its_own_templates() {
        // Review of 08ff0f9: every template was refused under this name.
        let dir = std::fs::canonicalize(std::env::temp_dir())
            .expect("tmp")
            .join(format!("mdkg-templates-firm-{}", std::process::id()));
        let set = dir.join(".mdkg/templates/default");
        std::fs::create_dir_all(&set).expect("mkdir");
        std::fs::write(dir.join(crate::config::CONFIG_PATH), "{}").expect("w");
        std::fs::write(set.join("task.md"), "---\ntype: task\n---\nLOCAL\n").expect("w");
        let firm = Path::new("/System/Volumes/Data").join(dir.strip_prefix("/").expect("abs"));
        if firm.join(crate::config::CONFIG_PATH).is_file() {
            let config = crate::config::load(&firm).expect("config");
            let (text, bundled) = template_text(&firm, &config, "task").expect("template");
            assert!(!bundled && text.contains("LOCAL"));
            load(&firm, &config).expect("load");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_template_falls_back_without_hanging() {
        // Review of 13b3e6e: reading it waited forever.
        let dir = std::env::temp_dir().join(format!(
            "mdkg-templates-fifo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let set = dir.join(".mdkg/templates/default");
        std::fs::create_dir_all(&set).expect("mkdir");
        std::fs::write(dir.join(crate::config::CONFIG_PATH), "{}").expect("w");
        let made = std::process::Command::new("mkfifo")
            .arg(set.join("task.md"))
            .status()
            .expect("mkfifo");
        assert!(made.success());
        let config = crate::config::load(&dir).expect("config");
        let (tx, rx) = std::sync::mpsc::channel();
        let (d, c) = (dir.clone(), config.clone());
        std::thread::spawn(move || {
            let _ = tx.send(template_text(&d, &c, "task").map(|(_, bundled)| bundled));
        });
        let bundled = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("hung on a FIFO")
            .expect("template");
        assert!(bundled, "the bundled template, as for an unreadable one");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod schema07_tests {
    use super::*;

    /// The bundled templates seed `new` in a format 0.7 graph, so every key
    /// they write must be one the node schema allows, with the same kind.
    #[test]
    fn bundled_templates_stay_inside_the_0_7_node_schema() {
        let schemas = mdkg_format::schema07::schemas();
        for node_type in ALLOWED_TYPES {
            let bundled = bundled_schema(node_type).expect("bundled template");
            let schema = &schemas[*node_type];
            for key in bundled.keys() {
                assert!(schema.allows(key), "{node_type}: {key} isn't in the schema");
                assert_eq!(schema.kind(key), bundled.kind(key), "{node_type}: {key}");
            }
        }
    }
}
