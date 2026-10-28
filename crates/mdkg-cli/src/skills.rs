//! `mdkg skills`: list, show and validate the skills in `.mdkg/skills/`,
//! and sync their mirrors. A skill is a folder holding a
//! `SKILL.md` whose frontmatter names it and says what it's for, as in
//! mdkg 0.6.0; `.agents/skills/` and `.claude/skills/` (or the folders
//! `customization.skill_mirrors.targets` names) hold copies for agents.
//!
//! Everything here reads, except `sync`, which leaves the writing to the
//! launcher's installer (mdkg-run), the one place mirrors are written.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{Config, ErrorKind, GraphError, config};
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{self, Report};

use mdkg_format::program::BIN;

/// Where skills live, relative to the root.
pub const SKILLS_DIR: &str = ".mdkg/skills";
/// Each mirror's list of the skills mdkg put there (0.6.0's format).
pub const MANAGED_FILE: &str = mdkg_run::install::MANAGED_LIST;

/// How deep a skill folder is read, and how many files: a skill is a few
/// files, so anything past these is refused, never read without end.
const MAX_DEPTH: usize = 16;
const MAX_FILES: usize = 10_000;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TagsMode {
    Any,
    All,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Comma-separated tags (such as `stage:plan`).
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,
    /// Whether skills need any or all of the tags (default any).
    #[arg(long, value_enum)]
    pub tags_mode: Option<TagsMode>,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    /// List the skills.
    #[command(after_help = examples!("skills list", "skills list --tags stage:plan --json"))]
    List(ListArgs),
    /// Show one skill: its frontmatter and its SKILL.md body.
    #[command(after_help = examples!("skills show mdkg-workflow", "skills show mdkg-workflow --meta"))]
    Show {
        /// The skill's slug (its folder name).
        slug: String,
        /// Omit the body.
        #[arg(long)]
        meta: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check every skill (or one) and that the mirrors match it.
    #[command(after_help = examples!("skills validate", "skills validate mdkg-workflow --json"))]
    Validate {
        /// Only this skill.
        slug: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add a skill the binary ships (`mdkg-workflow`) to `.mdkg/skills/`,
    /// and mirror it.
    #[command(after_help = examples!("skills add mdkg-workflow"))]
    Add {
        /// The shipped skill's slug.
        slug: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Copy `.mdkg/skills/` into the mirror folders agents read.
    #[command(after_help = examples!("skills sync --dry-run", "skills sync"))]
    Sync {
        /// Show what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl SkillsCommand {
    pub fn json(&self) -> bool {
        match self {
            SkillsCommand::List(a) => a.json,
            SkillsCommand::Show { json, .. }
            | SkillsCommand::Validate { json, .. }
            | SkillsCommand::Add { json, .. }
            | SkillsCommand::Sync { json, .. } => *json,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            SkillsCommand::List(_) => "skills.list",
            SkillsCommand::Show { .. } => "skills.show",
            SkillsCommand::Validate { .. } => "skills.validate",
            SkillsCommand::Add { .. } => "skills.add",
            SkillsCommand::Sync { .. } => "skills.sync",
        }
    }
}

pub fn run(root: &Path, command: &SkillsCommand) -> u8 {
    match command {
        SkillsCommand::List(a) => list(root, a),
        SkillsCommand::Show { slug, meta, json } => show(root, slug, *meta, *json),
        SkillsCommand::Validate { slug, json } => validate(root, slug.as_deref(), *json),
        SkillsCommand::Add { slug, json } => add(root, slug, *json),
        SkillsCommand::Sync { dry_run, json } => sync(root, *dry_run, *json),
    }
}

/// One skill, as its SKILL.md frontmatter has it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SkillRecord {
    /// Its folder's name, which nodes' `skills` lists use.
    pub slug: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub authors: Vec<String>,
    pub links: Vec<String>,
    /// The SKILL.md, relative to the root.
    pub path: String,
    /// Whether the skill has a `scripts/` folder.
    pub has_scripts: bool,
    /// Whether the skill has a `references/` folder.
    pub has_references: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillsListData {
    /// How many skills are listed.
    pub count: usize,
    pub skills: Vec<SkillRecord>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillShowData {
    pub skill: SkillRecord,
    /// The SKILL.md body after the frontmatter; absent with `--meta`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillsValidateData {
    /// How many skill folders were checked.
    pub checked: usize,
    pub errors: usize,
    pub warnings: usize,
    /// The one skill checked, when a slug was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// A skill folder, before its SKILL.md is read.
struct Folder {
    slug: String,
    dir: PathBuf,
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Whether a slug is kebab-case, as 0.6.0 requires.
pub fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// The skill folders under `.mdkg/skills/`, by slug. A link is not a skill
/// folder: the mirrors copy only skills whose files are in the project.
fn folders(root: &Path) -> Result<Vec<Folder>, GraphError> {
    let dir = root.join(SKILLS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(GraphError::failure(
                Code::IoReadFailed,
                format!("can't list {SKILLS_DIR}: {e}"),
            )
            .at(SKILLS_DIR, None));
        }
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !is_dir {
            continue;
        }
        let slug = entry.file_name().to_string_lossy().into_owned();
        found.push(Folder {
            slug,
            dir: entry.path(),
        });
    }
    found.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(found)
}

/// A skill folder's SKILL.md (or the legacy SKILLS.md), with layout
/// errors and warnings.
fn skill_file(root: &Path, f: &Folder, diags: &mut Vec<Diagnostic>) -> Option<PathBuf> {
    let canonical = f.dir.join("SKILL.md");
    let legacy = f.dir.join("SKILLS.md");
    let at = rel(root, &f.dir);
    match (canonical.is_file(), legacy.is_file()) {
        (true, true) => {
            diags.push(
                Diagnostic::error(Code::SkillBothFiles, "both SKILL.md and SKILLS.md exist")
                    .at(at, None),
            );
            None
        }
        (true, false) => Some(canonical),
        (false, true) => {
            diags.push(
                Diagnostic::warning(
                    Code::SkillLegacyFile,
                    "using the legacy SKILLS.md file name",
                )
                .at(rel(root, &legacy), None)
                .with_hint("rename the file to SKILL.md"),
            );
            Some(legacy)
        }
        (false, false) => {
            diags.push(Diagnostic::error(Code::SkillMissingFile, "missing SKILL.md").at(at, None));
            None
        }
    }
}

fn has_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Reads one skill: its record and its body, or the errors that stop it.
fn read_skill(
    root: &Path,
    config: &Config,
    f: &Folder,
    diags: &mut Vec<Diagnostic>,
) -> Option<(SkillRecord, String)> {
    if !is_slug(&f.slug) {
        diags.push(
            Diagnostic::error(
                Code::NodeBadSkillSlug,
                "a skill's folder name must be kebab-case",
            )
            .at(rel(root, &f.dir), None),
        );
        return None;
    }
    let file = skill_file(root, f, diags)?;
    let shown = rel(root, &file);
    let too_big = std::fs::metadata(&file).is_ok_and(|m| m.len() > config.limits.max_file_bytes);
    if too_big {
        diags.push(
            Diagnostic::error(
                Code::IndexLimitExceeded,
                format!(
                    "the file is over index.limits.max_file_bytes ({})",
                    config.limits.max_file_bytes
                ),
            )
            .at(shown, None),
        );
        return None;
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => {
            diags.push(
                Diagnostic::error(Code::IoReadFailed, format!("can't read the file: {e}"))
                    .at(shown, None),
            );
            return None;
        }
    };
    let parsed = match mdkg_format::frontmatter::parse(&text, &shown) {
        Ok(p) => p,
        Err(e) => {
            diags.push(
                GraphError::from_format(&e, root)
                    .diagnostic
                    .as_ref()
                    .clone(),
            );
            return None;
        }
    };
    let fm = &parsed.frontmatter;
    let mut problems = Vec::new();
    let mut text_field = |key: &str, required: bool| -> Option<String> {
        match fm.get(key) {
            Some(mdkg_format::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
            None if !required => None,
            _ => {
                problems.push(if required {
                    format!("{key} is required and must be a non-empty string")
                } else {
                    format!("{key} must be a non-empty string when set")
                });
                None
            }
        }
    };
    let name = text_field("name", true);
    let description = text_field("description", true);
    let version = text_field("version", false);
    let mut list = |key: &str| -> Vec<String> {
        match fm.get(key) {
            None => Vec::new(),
            Some(mdkg_format::Value::List(items)) => items.clone(),
            Some(_) => {
                problems.push(format!("{key} must be a list"));
                Vec::new()
            }
        }
    };
    let tags: Vec<String> = list("tags").iter().map(|t| t.to_lowercase()).collect();
    let authors: Vec<String> = list("authors").iter().map(|a| a.to_lowercase()).collect();
    let links = list("links");
    if !problems.is_empty() {
        diags.push(Diagnostic::error(Code::SkillInvalid, problems.join("; ")).at(shown, None));
        return None;
    }
    let (Some(name), Some(description)) = (name, description) else {
        return None;
    };
    Some((
        SkillRecord {
            slug: f.slug.clone(),
            name,
            description,
            tags,
            version,
            authors,
            links,
            path: shown,
            has_scripts: has_dir(&f.dir.join("scripts")),
            has_references: has_dir(&f.dir.join("references")),
        },
        parsed.body,
    ))
}

fn load_config(root: &Path) -> Result<Config, GraphError> {
    config::load(root)
}

fn not_found() -> GraphError {
    // The slug is typed text, so it isn't echoed in an error.
    GraphError::not_found(
        Code::SkillNotFound,
        format!("no such skill in {SKILLS_DIR}"),
    )
    .with_hint(format!("`{BIN} skills list` lists the skills"))
}

pub fn list(root: &Path, args: &ListArgs) -> u8 {
    let report = (|| -> Result<Report<SkillsListData>, GraphError> {
        let config = load_config(root)?;
        let wanted: Vec<String> = args
            .tags
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let all = matches!(args.tags_mode, Some(TagsMode::All));
        let mut diags = Vec::new();
        let mut skills = Vec::new();
        for f in folders(root)? {
            if let Some((skill, _)) = read_skill(root, &config, &f, &mut diags) {
                let has = |t: &String| skill.tags.contains(t);
                let keep = wanted.is_empty()
                    || if all {
                        wanted.iter().all(has)
                    } else {
                        wanted.iter().any(has)
                    };
                if keep {
                    skills.push(skill);
                }
            }
        }
        // A skill that can't be read is skipped here, with its problem
        // as a warning: `skills validate` is where it fails.
        let warned = diags.into_iter().map(|mut d| {
            d.severity = mdkg_format::Severity::Warning;
            d
        });
        Ok(Report::ok(SkillsListData {
            count: skills.len(),
            skills,
        })
        .with(warned))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("skills.list", args.json, report, |d| {
        if d.skills.is_empty() {
            println!("no skills in {SKILLS_DIR}");
        }
        for s in &d.skills {
            let tags = if s.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", s.tags.join(", "))
            };
            println!(
                "{}{}: {}",
                s.slug,
                output::shown_path(&tags),
                output::shown_path(&s.description)
            );
        }
    })
}

pub fn show(root: &Path, slug: &str, meta: bool, json: bool) -> u8 {
    let report = (|| -> Result<Report<SkillShowData>, GraphError> {
        let config = load_config(root)?;
        let f = folders(root)?
            .into_iter()
            .find(|f| f.slug == slug)
            .ok_or_else(not_found)?;
        let mut diags = Vec::new();
        match read_skill(root, &config, &f, &mut diags) {
            Some((skill, body)) => Ok(Report::ok(SkillShowData {
                skill,
                body: (!meta).then_some(body),
            })
            .with(diags)),
            None => Ok(Report {
                data: None,
                diagnostics: diags,
                failure: Some(ErrorKind::Validation),
            }),
        }
    })()
    .unwrap_or_else(Report::failed);
    output::emit("skills.show", json, report, |d| {
        let s = &d.skill;
        println!("{} ({})", s.slug, s.path);
        println!("name: {}", output::shown_path(&s.name));
        println!("description: {}", output::shown_path(&s.description));
        if let Some(v) = &s.version {
            println!("version: {}", output::shown_path(v));
        }
        for (label, items) in [
            ("tags", &s.tags),
            ("authors", &s.authors),
            ("links", &s.links),
        ] {
            if !items.is_empty() {
                println!("{label}: {}", output::shown_path(&items.join(", ")));
            }
        }
        if let Some(body) = &d.body {
            println!();
            print!("{}", output::shown_body(body));
        }
    })
}

pub fn validate(root: &Path, slug: Option<&str>, json: bool) -> u8 {
    let report = (|| -> Result<Report<SkillsValidateData>, GraphError> {
        let config = load_config(root)?;
        let mut all = folders(root)?;
        if let Some(slug) = slug {
            all.retain(|f| f.slug == slug);
            if all.is_empty() {
                return Err(not_found());
            }
        }
        let mut diags = Vec::new();
        for f in &all {
            read_skill(root, &config, f, &mut diags);
        }
        diags.extend(mirror_check(root, &config, slug));
        let errors = diags
            .iter()
            .filter(|d| d.severity == mdkg_format::Severity::Error)
            .count();
        let data = SkillsValidateData {
            checked: all.len(),
            errors,
            warnings: diags.len() - errors,
            target: slug.map(String::from),
        };
        Ok(Report {
            data: Some(data),
            diagnostics: diags,
            failure: (errors > 0).then_some(ErrorKind::Validation),
        })
    })()
    .unwrap_or_else(Report::failed);
    output::emit("skills.validate", json, report, |d| {
        let what = match &d.target {
            Some(t) => format!("{t} (1 skill checked)"),
            None => format!(
                "{} skill{} checked",
                d.checked,
                if d.checked == 1 { "" } else { "s" }
            ),
        };
        if d.errors == 0 {
            println!("skills ok: {what}, {} warning(s)", d.warnings);
        } else {
            println!(
                "skills failed: {what}, {} error(s), {} warning(s)",
                d.errors, d.warnings
            );
        }
    })
}

/// Every file under a folder, relative to it, without following links;
/// `None` when the folder isn't there or holds more than a skill should.
fn files_under(dir: &Path) -> Option<BTreeSet<PathBuf>> {
    fn walk(base: &Path, dir: &Path, depth: usize, out: &mut BTreeSet<PathBuf>) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let t = entry.file_type().ok()?;
            let path = entry.path();
            if t.is_dir() {
                walk(base, &path, depth + 1, out)?;
            } else {
                // Files and links alike: a link in a mirror is drift too.
                out.insert(path.strip_prefix(base).ok()?.to_path_buf());
                if out.len() > MAX_FILES {
                    return None;
                }
            }
        }
        Some(())
    }
    if !has_dir(dir) {
        return None;
    }
    let mut out = BTreeSet::new();
    walk(dir, dir, 0, &mut out)?;
    Some(out)
}

/// Whether two files have the same bytes; a link compares as different
/// from anything, since the installer never writes one.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let plain = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file());
    if !plain(a) || !plain(b) {
        return false;
    }
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) if x.len() != y.len() => return false,
        (Ok(_), Ok(_)) => {}
        _ => return false,
    }
    matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// The skills a mirror's `.mdkg-managed.json` lists, if it has one.
fn managed(target: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(target.join(MANAGED_FILE)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(
        v.get("managed_slugs")?
            .as_array()?
            .iter()
            .filter_map(|s| s.as_str().map(String::from))
            .collect(),
    )
}

/// Whether each mirror matches `.mdkg/skills/`: read-only, warnings only
/// (a mirror is a copy, so `skills sync` fixes all of these).
pub fn mirror_check(root: &Path, config: &Config, only: Option<&str>) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let sources: Vec<Folder> = match folders(root) {
        Ok(f) => f
            .into_iter()
            .filter(|f| is_slug(&f.slug) && only.is_none_or(|o| o == f.slug))
            .collect(),
        Err(_) => return diags,
    };
    let fix = format!("run `{BIN} skills sync`");
    for target in &config.skill_mirror_targets {
        let dir = root.join(target);
        if !has_dir(&dir) {
            if !sources.is_empty() {
                diags.push(
                    Diagnostic::warning(Code::SkillMirrorMissing, "skill mirror folder is missing")
                        .at(target.clone(), None)
                        .with_hint(fix.clone()),
                );
            }
            continue;
        }
        for s in &sources {
            let mirror = dir.join(&s.slug);
            let shown = rel(root, &mirror);
            let (Some(want), have) = (files_under(&s.dir), files_under(&mirror)) else {
                continue;
            };
            let Some(have) = have else {
                diags.push(
                    Diagnostic::warning(Code::SkillMirrorDrift, "the mirror lacks this skill")
                        .at(shown, None)
                        .with_hint(fix.clone()),
                );
                continue;
            };
            let differs = want != have
                || want
                    .iter()
                    .any(|f| !same_bytes(&s.dir.join(f), &mirror.join(f)));
            if differs {
                diags.push(
                    Diagnostic::warning(
                        Code::SkillMirrorDrift,
                        "the mirrored skill differs from its source in .mdkg/skills",
                    )
                    .at(shown, None)
                    .with_hint(format!(
                        "edit the .mdkg/skills copy, then run `{BIN} skills sync`"
                    )),
                );
            }
        }
        if only.is_none() {
            match managed(&dir) {
                None => diags.push(
                    Diagnostic::warning(
                        Code::SkillMirrorManifestMissing,
                        "skill mirror manifest is missing",
                    )
                    .at(rel(root, &dir.join(MANAGED_FILE)), None)
                    .with_hint(fix.clone()),
                ),
                Some(slugs) => {
                    for slug in slugs {
                        if is_slug(&slug)
                            && !sources.iter().any(|s| s.slug == slug)
                            && has_dir(&dir.join(&slug))
                        {
                            diags.push(
                                Diagnostic::warning(
                                    Code::SkillMirrorStale,
                                    "a mirrored skill whose source is gone",
                                )
                                .at(rel(root, &dir.join(&slug)), None)
                                .with_hint(fix.clone()),
                            );
                        }
                    }
                }
            }
        }
    }
    diags
}

pub fn sync(root: &Path, dry_run: bool, json: bool) -> u8 {
    let report = sync_report(root, dry_run)
        .map(Report::ok)
        .unwrap_or_else(Report::failed);
    output::emit("skills.sync", json, report, |d| {
        let changed: Vec<_> = d
            .changes
            .iter()
            .filter(|c| c.action != mdkg_run::install::Action::Unchanged)
            .collect();
        if changed.is_empty() {
            println!("skill mirrors are up to date");
        }
        for c in changed {
            let verb = match c.action {
                mdkg_run::install::Action::Created => "created",
                mdkg_run::install::Action::Updated => "updated",
                mdkg_run::install::Action::Removed => "removed",
                mdkg_run::install::Action::Unchanged => "unchanged",
            };
            println!(
                "{}{verb} {}",
                if dry_run { "would be " } else { "" },
                output::shown_path(&c.path)
            );
        }
        for n in &d.notes {
            println!("note: {}", output::shown_path(n));
        }
    })
}

/// Install's undo folder for the worktree at `root`, or `None` outside
/// git: what `skills sync` and `init` change is recorded there, so
/// `install --undo` still puts every file back as it was before install
/// first changed it.
pub(crate) fn install_undo_dir(root: &Path) -> Option<PathBuf> {
    let repo = crate::coord::Repo::open(root).ok()?;
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .and_then(std::fs::canonicalize)
        .ok()?;
    Some(mdkg_run::install::undo_dir(
        &common.join("mdkg").join("install"),
        &repo.top,
    ))
}

/// A launcher error as the CLI reports it.
pub(crate) fn graph_error(e: mdkg_run::RunError) -> GraphError {
    let kind = match e.kind {
        mdkg_run::ErrorKind::Usage => ErrorKind::Usage,
        mdkg_run::ErrorKind::NotFound => ErrorKind::NotFound,
        mdkg_run::ErrorKind::Failure => ErrorKind::Failure,
        mdkg_run::ErrorKind::Conflict => ErrorKind::Conflict,
    };
    let mut d = Diagnostic::error(
        Code::parse(&e.code).unwrap_or(Code::IoWriteFailed),
        e.message,
    );
    d.hint = e.hint;
    d.path = e.path;
    GraphError::new(kind, d)
}

/// The installer's skill-mirror sync, after checking the config's mirror
/// folders are the two it writes.
fn sync_report(root: &Path, dry_run: bool) -> Result<mdkg_run::install::InstallReport, GraphError> {
    let config = load_config(root)?;
    // A config that lists its projections names only mirrors sync writes
    // (`skills:.agents/skills`, `skills:.claude/skills`), any of them.
    let declared = mdkg_run::projection::Projections::read(root)
        .map_err(graph_error)?
        .declared();
    // Without the list, the installer writes these two only; a config
    // naming others would have sync succeed while the folders it names
    // stay missing.
    let mut targets: Vec<&str> = config
        .skill_mirror_targets
        .iter()
        .map(|t| t.trim_end_matches('/'))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    if !declared && targets != [".agents/skills", ".claude/skills"] {
        return Err(GraphError::usage(
            Code::SkillMirrorTargetsUnsupported,
            "skills sync writes .agents/skills and .claude/skills, and customization.skill_mirrors.targets names other folders",
        )
        .at(config::CONFIG_PATH, None)
        .with_hint("set customization.skill_mirrors.targets to [\".agents/skills\", \".claude/skills\"], or copy the skills yourself"));
    }
    let repo = crate::coord::Repo::open(root)?;
    // As install: the mirrors sit beside the graph at the worktree's top.
    if !repo.prefix.is_empty() {
        return Err(GraphError::usage(
            Code::InstallUnsupportedLayout,
            format!(
                "the graph is in {} below the worktree's top; skill mirrors need it at the top",
                repo.prefix.trim_end_matches('/')
            ),
        ));
    }
    mdkg_run::install::sync_skills(&repo.top, dry_run, install_undo_dir(root).as_deref())
        .map_err(graph_error)
}

/// The skills the binary ships, by slug.
pub fn bundled(slug: &str) -> Option<&'static str> {
    (slug == crate::init::WORKFLOW_SLUG).then_some(crate::init::WORKFLOW_SKILL)
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillAddData {
    pub slug: String,
    /// The SKILL.md written, relative to the root.
    pub path: String,
    /// `created`, or `kept` when it was already there as shipped.
    pub action: String,
    /// The mirror files the sync wrote.
    pub mirrored: Vec<mdkg_run::install::Change>,
}

/// `skills add <slug>`: writes a skill the binary ships into
/// `.mdkg/skills/`, then syncs the mirrors. An existing SKILL.md that
/// differs is the project's own, so it's refused, never replaced.
pub fn add(root: &Path, slug: &str, json: bool) -> u8 {
    let report = (|| -> Result<Report<SkillAddData>, GraphError> {
        load_config(root)?;
        let Some(text) = bundled(slug) else {
            return Err(GraphError::not_found(
                Code::SkillNotFound,
                "the binary ships no skill by that name",
            )
            .with_hint(format!("shipped skills: {}", crate::init::WORKFLOW_SLUG)));
        };
        let rel = format!("{SKILLS_DIR}/{slug}/SKILL.md");
        let path = root.join(&rel);
        let action = match std::fs::read(&path) {
            Ok(have) if have == text.as_bytes() => "kept",
            Ok(_) => {
                return Err(GraphError::usage(
                    Code::SkillExists,
                    "the skill is already here, changed from what the binary ships",
                )
                .at(rel, None)
                .with_hint("edit it in place; move it aside first to take the shipped copy"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if crate::init::publish_new(root, &rel, text)? {
                    "created"
                } else {
                    return Err(GraphError::conflict(
                        Code::SkillExists,
                        "the skill appeared while add ran",
                    )
                    .at(rel, None));
                }
            }
            Err(e) => {
                return Err(GraphError::failure(
                    Code::IoReadFailed,
                    format!("can't read {rel}: {e}"),
                ));
            }
        };
        let synced = sync_report(root, false)?;
        Ok(Report::ok(SkillAddData {
            slug: slug.into(),
            path: rel,
            action: action.into(),
            mirrored: synced
                .changes
                .into_iter()
                .filter(|c| c.action != mdkg_run::install::Action::Unchanged)
                .collect(),
        }))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("skills.add", json, report, |d| {
        println!("{} {}", d.action, output::shown_path(&d.path));
        for c in &d.mirrored {
            println!("mirrored {}", output::shown_path(&c.path));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_kebab_case() {
        assert!(is_slug("pursue-mdkg-goal"));
        assert!(is_slug("a1"));
        assert!(!is_slug(""));
        assert!(!is_slug("-a"));
        assert!(!is_slug("a--b"));
        assert!(!is_slug("A"));
        assert!(!is_slug("a_b"));
    }
}
