//! The Go adapter: maps changed paths to the packages whose tests need to
//! run, from `go list -json ./...` in every module of the repository.
//!
//! - A `.go` file maps to its folder's package; `_test.go` files and
//!   external `_test` packages belong to the same package.
//! - Other files map to a package only when they sit directly in its
//!   folder (not the module's top folder, where a Makefile or CI script
//!   sits), under its `testdata/`, or among its embedded files. Anything
//!   else (a `migrations/` folder read by tests elsewhere, say) is left to
//!   the config rules, or forces a full run.
//! - The graph doesn't depend on the machine's build settings: `go list`
//!   runs for linux/amd64 with cgo on, the imports of files build
//!   constraints exclude even then (another OS, a tag) are read from their
//!   source and count too, and a folder of Go files that `go list` doesn't
//!   report at all (every file excluded, like an `integration`-tagged test
//!   package) makes the run full when the change reaches anything it
//!   imports. Files marked `//go:build ignore` never build, and don't count.
//! - A package's dependents are the packages that import it, transitively,
//!   plus every package whose tests import one of those. Test imports end
//!   the walk: a package's tests importing X doesn't make that package's
//!   importers depend on X.
//! - `go list` runs once per module, with no go env file, no downloads and
//!   no private-module exceptions; any error, or an incomplete package,
//!   makes the run full.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::MatchKind;
use super::select::{Adapter, Mapping, Target};

/// One package from `go list -json`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListedPackage {
    import_path: String,
    dir: PathBuf,
    #[serde(default)]
    imports: Vec<String>,
    #[serde(default)]
    test_imports: Vec<String>,
    #[serde(default)]
    x_test_imports: Vec<String>,
    #[serde(default)]
    embed_files: Vec<String>,
    #[serde(default)]
    test_embed_files: Vec<String>,
    #[serde(default)]
    x_test_embed_files: Vec<String>,
    /// Files build constraints exclude on this machine.
    #[serde(default)]
    ignored_go_files: Vec<String>,
    /// Set when go list couldn't load the package.
    #[serde(default)]
    error: Option<serde_json::Value>,
    /// Set when the package or one of its dependencies has an error.
    #[serde(default)]
    incomplete: bool,
    #[serde(default)]
    deps_errors: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct Package {
    module: String,
    module_dir: String,
}

#[derive(Debug, Default)]
struct Graph {
    /// Import path to package.
    packages: BTreeMap<String, Package>,
    /// Repository-relative folder to import path.
    by_dir: BTreeMap<String, String>,
    /// Repository-relative embedded file to the packages embedding it.
    embedded: BTreeMap<String, BTreeSet<String>>,
    /// Directories of packages with embeds. A removed descendant may have
    /// matched a pattern whose expanded head file list no longer includes it.
    embed_dirs: BTreeSet<String>,
    /// Import path to the packages that import it (not in tests).
    importers: BTreeMap<String, BTreeSet<String>>,
    /// Import path to the packages whose tests import it.
    test_importers: BTreeMap<String, BTreeSet<String>>,
    /// Every module's top folder.
    module_dirs: BTreeSet<String>,
    /// Import path to the folders `go list` didn't report whose files
    /// import it.
    unlisted_importers: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone)]
struct Module {
    /// Repository-relative folder holding `go.mod` (`.` for the top).
    dir: String,
    /// Preserve the committed spelling for preflight before any Go command.
    manifest: String,
}

pub struct GoAdapter {
    top: PathBuf,
    /// An empty private folder for go's build cache.
    cache: PathBuf,
    modules: Vec<Module>,
    /// Every path in the head's tree.
    tracked: Vec<String>,
    graph: Option<Result<Graph, String>>,
}

/// The repository-relative folder of a path (`.` for a top-level file).
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(d, _)| d)
}

/// Whether `path` lies in folder `dir` (`.` holds everything).
fn within(dir: &str, path: &str) -> bool {
    dir == "."
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn ignored_by_go(segment: &str) -> bool {
    segment == "testdata"
        || segment == "vendor"
        || segment.starts_with('.')
        || segment.starts_with('_')
}

/// Snapshot and adapter discovery must recognize the same module filenames.
/// Directory filtering remains case-sensitive, matching Go's traversal rules.
pub fn is_module_path(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("go.mod"))
}

impl GoAdapter {
    /// The adapter for a repository: one module per tracked `go.mod`
    /// outside folders Go ignores (`testdata`, `vendor`, `.` and `_`
    /// prefixes). `None` when the repository has no Go module.
    pub fn discover(top: &Path, cache: &Path, tracked: &[String]) -> Option<Self> {
        let mut modules: Vec<Module> = tracked
            .iter()
            .filter(|p| is_module_path(p))
            .filter(|p| !parent(p).split('/').any(ignored_by_go) || parent(p) == ".")
            .map(|p| Module {
                dir: parent(p).to_string(),
                manifest: p.clone(),
            })
            .collect();
        if modules.is_empty() {
            return None;
        }
        // Innermost first, so a path goes to the deepest module holding it.
        modules.sort_by(|a, b| b.dir.len().cmp(&a.dir.len()).then(a.dir.cmp(&b.dir)));
        Some(Self {
            top: top.to_path_buf(),
            cache: cache.to_path_buf(),
            modules,
            tracked: tracked.to_vec(),
            graph: None,
        })
    }

    fn module_of(&self, path: &str) -> Option<&Module> {
        self.modules.iter().find(|m| within(&m.dir, path))
    }

    fn graph(&mut self) -> Result<&Graph, String> {
        if self.graph.is_none() {
            self.graph = Some(
                load(&self.top, &self.cache, &self.modules).and_then(|g| self.add_unlisted(g)),
            );
        }
        match self.graph.as_ref() {
            Some(Ok(g)) => Ok(g),
            Some(Err(e)) => Err(e.clone()),
            None => Err("the Go graph didn't load".into()),
        }
    }
}

impl GoAdapter {
    /// Records the folders of Go files `go list` didn't report (every file
    /// excluded by build constraints), with what they import.
    fn add_unlisted(&self, mut graph: Graph) -> Result<Graph, String> {
        let mut dirs: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for path in &self.tracked {
            let name = path.rsplit('/').next().unwrap_or(path);
            if !name.ends_with(".go") || name.starts_with('_') || name.starts_with('.') {
                continue;
            }
            let Some(module) = self.module_of(path) else {
                continue;
            };
            let dir = parent(path);
            let inside = if module.dir == "." {
                dir
            } else {
                dir.strip_prefix(&module.dir)
                    .map_or("", |r| r.trim_start_matches('/'))
            };
            // `.` is the module's own top folder, not a dot-folder Go skips.
            if (inside != "." && inside.split('/').any(ignored_by_go))
                || graph.by_dir.contains_key(dir)
            {
                continue;
            }
            dirs.entry(dir.to_string()).or_default().push(path);
        }
        for (dir, files) in dirs {
            for file in files {
                let source = std::fs::read(self.top.join(file))
                    .map_err(|e| format!("can't read {file}: {e}"))?;
                if never_builds(&source) {
                    continue;
                }
                let imports = parse_imports(&source)
                    .map_err(|e| format!("can't read the imports of {file}: {e}"))?;
                if imports.iter().any(|i| i == "embed") {
                    return Err(format!(
                        "{file} embeds files under build settings go list didn't inspect"
                    ));
                }
                for import in imports {
                    graph
                        .unlisted_importers
                        .entry(import)
                        .or_default()
                        .insert(dir.clone());
                }
            }
        }
        Ok(graph)
    }
}

fn load(top: &Path, cache: &Path, modules: &[Module]) -> Result<Graph, String> {
    // Check all modules first, so a canonical module cannot invoke Go before
    // a later noncanonical module is refused. Fail consistently on every host.
    if let Some(module) = modules
        .iter()
        .find(|m| m.manifest.rsplit('/').next() != Some("go.mod"))
    {
        return Err(format!(
            "{} has noncanonical casing; Go may open it as go.mod on a case-insensitive filesystem",
            module.manifest
        ));
    }
    let mut graph = Graph::default();
    let mut ordered: Vec<&Module> = modules.iter().collect();
    ordered.sort_by(|a, b| a.dir.cmp(&b.dir));
    for module in ordered {
        let dir = if module.dir == "." {
            top.to_path_buf()
        } else {
            top.join(&module.dir)
        };
        let module_path = module_path(&dir)?;
        graph.module_dirs.insert(module.dir.clone());
        // The written tree's own go.work, or none: never one found by
        // walking up from the temporary folder. No downloads: a module or
        // toolchain that isn't local fails the load, and the run is full.
        let gowork = top.join("go.work");
        let home = private_go_home(cache)?;
        let out = Command::new("go")
            // Only the fields read here: asking for none of the build's
            // own (Stale and the like) keeps go list from planning a build,
            // which would run the C compiler for cgo packages.
            .args([
                "list",
                "-json=ImportPath,Dir,Imports,TestImports,XTestImports,EmbedFiles,TestEmbedFiles,XTestEmbedFiles,IgnoredGoFiles,Error,Incomplete,DepsErrors",
                "./...",
            ])
            .current_dir(&dir)
            .env(
                "GOWORK",
                if gowork.is_file() {
                    gowork.as_os_str()
                } else {
                    "off".as_ref()
                },
            )
            .env("GOENV", "off")
            .env("GO111MODULE", "on")
            .env("GOCACHE", cache)
            // One platform everywhere, so every machine lists the same
            // packages; other platforms' files count through their imports.
            .env("GOOS", "linux")
            .env("GOARCH", "amd64")
            .env("GOAMD64", "v1")
            .env("GOTMPDIR", cache)
            // go writes telemetry counters under the user's config folder,
            // whatever the environment says (GOTELEMETRY is read-only): a
            // private HOME keeps them in the private folder, and the
            // caller's module cache is passed on explicitly.
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", &home)
            .env("APPDATA", &home)
            .env("CGO_ENABLED", "1")
            .env("GOEXPERIMENT", "")
            .env("GOPROXY", "off")
            .env("GOPRIVATE", "")
            .env("GONOPROXY", "")
            .env("GONOSUMDB", "")
            .env("GOVCS", "*:off")
            .env("GOTOOLCHAIN", "local")
            // Go's own defaults (vendor when vendor/modules.txt exists),
            // except that no VCS stamping runs git: with GOENV off, nothing
            // else sets flags or tags.
            .env("GOFLAGS", "-buildvcs=false")
            .envs(caller_go_paths())
            .output()
            .map_err(|e| format!("can't run go list in {}: {e}", module.dir))?;
        if !out.status.success() {
            return Err(format!(
                "go list -json ./... in {} failed: {}",
                module.dir,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let stream = serde_json::Deserializer::from_slice(&out.stdout).into_iter::<ListedPackage>();
        for listed in stream {
            let listed = listed
                .map_err(|e| format!("go list output in {} is unreadable: {e}", module.dir))?;
            if let Some(error) = &listed.error {
                return Err(format!(
                    "go list couldn't load {}: {error}",
                    listed.import_path
                ));
            }
            if listed.incomplete || !listed.deps_errors.is_empty() {
                return Err(format!(
                    "go list loaded {} incompletely: {}",
                    listed.import_path,
                    serde_json::Value::Array(listed.deps_errors.clone())
                ));
            }
            if listed.import_path != module_path
                && !listed
                    .import_path
                    .strip_prefix(module_path.as_str())
                    .is_some_and(|r| r.starts_with('/'))
            {
                return Err(format!(
                    "go list reported {} outside module {module_path}",
                    listed.import_path
                ));
            }
            let dir = relative(top, &listed.dir)?;
            // Files excluded here still build elsewhere (another OS, a tag):
            // their imports count, read from their source.
            let mut ignored_imports = Vec::new();
            let mut ignored_test_imports = Vec::new();
            for file in &listed.ignored_go_files {
                let source = std::fs::read(listed.dir.join(file))
                    .map_err(|e| format!("can't read {dir}/{file}: {e}"))?;
                if never_builds(&source) {
                    continue;
                }
                let imports = parse_imports(&source)
                    .map_err(|e| format!("can't read the imports of {dir}/{file}: {e}"))?;
                if imports.iter().any(|i| i == "embed") {
                    return Err(format!(
                        "{dir}/{file} embeds files under build settings go list didn't inspect"
                    ));
                }
                if file.ends_with("_test.go") {
                    ignored_test_imports.extend(imports);
                } else {
                    ignored_imports.extend(imports);
                }
            }
            for files in [
                &listed.embed_files,
                &listed.test_embed_files,
                &listed.x_test_embed_files,
            ] {
                if !files.is_empty() {
                    graph.embed_dirs.insert(dir.clone());
                }
                for f in files {
                    let path = if dir == "." {
                        f.clone()
                    } else {
                        format!("{dir}/{f}")
                    };
                    graph
                        .embedded
                        .entry(path)
                        .or_default()
                        .insert(listed.import_path.clone());
                }
            }
            for i in listed.imports.iter().chain(&ignored_imports) {
                graph
                    .importers
                    .entry(i.clone())
                    .or_default()
                    .insert(listed.import_path.clone());
            }
            for i in listed
                .test_imports
                .iter()
                .chain(&listed.x_test_imports)
                .chain(&ignored_test_imports)
            {
                graph
                    .test_importers
                    .entry(i.clone())
                    .or_default()
                    .insert(listed.import_path.clone());
            }
            if let Some(previous) = graph.by_dir.insert(dir.clone(), listed.import_path.clone()) {
                return Err(format!(
                    "go list reported two packages in {dir}: {previous} and {}",
                    listed.import_path
                ));
            }
            if let Some(previous) = graph.packages.insert(
                listed.import_path,
                Package {
                    module: module_path.clone(),
                    module_dir: module.dir.clone(),
                },
            ) {
                return Err(format!(
                    "Go modules {} and {} report the same package import path",
                    previous.module_dir, module.dir
                ));
            }
        }
    }
    Ok(graph)
}

/// The caller's GOPATH and module cache, as go would find them with no env
/// file: the variables, else `<home>/go` and its `pkg/mod`. With no home
/// known, neither is set, and go finds none (its own error, so full).
fn caller_go_paths() -> Vec<(&'static str, std::ffi::OsString)> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()));
    let gopath = std::env::var_os("GOPATH")
        .filter(|p| !p.is_empty())
        .or_else(|| home.map(|h| Path::new(&h).join("go").into_os_string()));
    let Some(gopath) = gopath else {
        return Vec::new();
    };
    let gomodcache = std::env::var_os("GOMODCACHE")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| {
            let first = std::env::split_paths(&gopath).next().unwrap_or_default();
            first.join("pkg").join("mod").into_os_string()
        });
    vec![("GOPATH", gopath), ("GOMODCACHE", gomodcache)]
}

/// A private home for go, with telemetry turned off where go reads its
/// mode on each platform, so go neither counts nor starts its uploader.
fn private_go_home(cache: &Path) -> Result<PathBuf, String> {
    let home = cache.join("home");
    for dir in [
        home.join("Library/Application Support/go/telemetry"),
        home.join("go/telemetry"),
    ] {
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join("mode"), "off"))
            .map_err(|e| format!("can't turn go's telemetry off in {}: {e}", dir.display()))?;
    }
    Ok(home)
}

/// Whether a Go file is marked to never build (`//go:build ignore`, the
/// convention for generators and templates): its build line, before the
/// package clause, is exactly `ignore`.
fn never_builds(source: &[u8]) -> bool {
    let text = String::from_utf8_lossy(source);
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        // Go reads `//go:build` only when whitespace (or nothing) follows.
        if let Some(expr) = line
            .strip_prefix("//go:build")
            .filter(|e| e.is_empty() || e.starts_with(char::is_whitespace))
        {
            return expr.trim() == "ignore";
        }
        if !(line.is_empty() || line.starts_with("//")) {
            return false;
        }
    }
    false
}

/// The import paths a Go source file declares, read the way the compiler
/// reads its header: the `package` clause, then every `import` declaration,
/// with comments anywhere. Anything else ends the header.
fn parse_imports(source: &[u8]) -> Result<Vec<String>, String> {
    let text = std::str::from_utf8(source).map_err(|_| "the file isn't UTF-8".to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lexer = Lexer {
        rest: text,
        semi_ok: false,
    };
    let mut first = lexer.token()?;
    while first == Some(Token::Semi) {
        first = lexer.token()?;
    }
    if first != Some(Token::Word("package")) {
        return Err("no package clause".into());
    }
    match lexer.token()? {
        Some(Token::Word(_)) => {}
        _ => return Err("the package clause has no name".into()),
    }
    let mut imports = Vec::new();
    loop {
        match lexer.token()? {
            Some(Token::Semi) => continue,
            Some(Token::Word("import")) => {}
            _ => return Ok(imports),
        }
        match lexer.token()? {
            Some(Token::Open) => loop {
                match lexer.token()? {
                    Some(Token::Semi) => {}
                    Some(Token::Close) => break,
                    Some(Token::Str(path)) => imports.push(path),
                    Some(Token::Word(_) | Token::Dot) => match lexer.token()? {
                        Some(Token::Str(path)) => imports.push(path),
                        _ => return Err("an import has a name but no path".into()),
                    },
                    _ => return Err("an import block isn't closed".into()),
                }
            },
            Some(Token::Str(path)) => imports.push(path),
            Some(Token::Word(_) | Token::Dot) => match lexer.token()? {
                Some(Token::Str(path)) => imports.push(path),
                _ => return Err("an import has a name but no path".into()),
            },
            _ => return Err("an import declaration has no path".into()),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Token<'a> {
    Word(&'a str),
    Str(String),
    Dot,
    Open,
    Close,
    /// `;`, or a newline (Go's automatic semicolons, near enough here).
    Semi,
    Other,
}

struct Lexer<'a> {
    rest: &'a str,
    /// Whether a newline here ends a statement: Go inserts a semicolon
    /// only after an identifier (not `package` or `import`), a literal or
    /// a closing parenthesis.
    semi_ok: bool,
}

impl<'a> Lexer<'a> {
    fn token(&mut self) -> Result<Option<Token<'a>>, String> {
        let token = self.raw_token()?;
        self.semi_ok = match &token {
            Some(Token::Word(w)) => *w != "package" && *w != "import",
            Some(Token::Str(_) | Token::Close) => true,
            _ => false,
        };
        Ok(token)
    }

    fn raw_token(&mut self) -> Result<Option<Token<'a>>, String> {
        loop {
            let Some(c) = self.rest.chars().next() else {
                return Ok(None);
            };
            if c == ';' || (c == '\n' && self.semi_ok) {
                self.rest = &self.rest[1..];
                return Ok(Some(Token::Semi));
            }
            if c.is_whitespace() {
                self.rest = &self.rest[c.len_utf8()..];
                continue;
            }
            if let Some(after) = self.rest.strip_prefix("//") {
                self.rest = after.find('\n').map_or("", |i| &after[i..]);
                continue;
            }
            if let Some(after) = self.rest.strip_prefix("/*") {
                let end = after.find("*/").ok_or("a comment isn't closed")?;
                let comment = &after[..end];
                self.rest = &after[end + 2..];
                if comment.contains('\n') && self.semi_ok {
                    return Ok(Some(Token::Semi));
                }
                continue;
            }
            return Ok(Some(match c {
                '(' => {
                    self.rest = &self.rest[1..];
                    Token::Open
                }
                ')' => {
                    self.rest = &self.rest[1..];
                    Token::Close
                }
                '.' => {
                    self.rest = &self.rest[1..];
                    Token::Dot
                }
                '"' => {
                    let body = &self.rest[1..];
                    let mut out = String::new();
                    let mut chars = body.char_indices();
                    loop {
                        match chars.next() {
                            Some((i, '"')) => {
                                self.rest = &body[i + 1..];
                                break;
                            }
                            Some((_, '\\')) => match chars.next() {
                                Some((_, 'x')) => {
                                    let hex: String =
                                        chars.by_ref().take(2).map(|(_, h)| h).collect();
                                    let byte = u8::from_str_radix(&hex, 16)
                                        .map_err(|_| "a bad \\x escape")?;
                                    if !byte.is_ascii() {
                                        return Err("an import path escape isn't ASCII".into());
                                    }
                                    out.push(byte as char);
                                }
                                Some((_, e @ ('\\' | '"'))) => out.push(e),
                                _ => return Err("an import path has an unknown escape".into()),
                            },
                            Some((_, '\n')) | None => {
                                return Err("an import path isn't closed".into());
                            }
                            Some((_, ch)) => out.push(ch),
                        }
                    }
                    Token::Str(out)
                }
                '`' => {
                    let body = &self.rest[1..];
                    let end = body.find('`').ok_or("a raw string isn't closed")?;
                    self.rest = &body[end + 1..];
                    Token::Str(body[..end].to_string())
                }
                c if c.is_alphanumeric() || c == '_' => {
                    let end = self
                        .rest
                        .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                        .unwrap_or(self.rest.len());
                    let word = &self.rest[..end];
                    self.rest = &self.rest[end..];
                    Token::Word(word)
                }
                _ => Token::Other,
            }));
        }
    }
}

/// The module path from `go.mod`'s `module` line.
fn module_path(dir: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(dir.join("go.mod"))
        .map_err(|e| format!("can't read {}: {e}", dir.join("go.mod").display()))?;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or_default().trim();
        if let Some(rest) = line.strip_prefix("module") {
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() && rest.starts_with(char::is_whitespace) {
                return Ok(name.to_string());
            }
        }
    }
    Err(format!(
        "{} has no module line",
        dir.join("go.mod").display()
    ))
}

/// A folder `go list` reported, relative to the repository.
fn relative(top: &Path, dir: &Path) -> Result<String, String> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let rel = dir
        .strip_prefix(top)
        .map_err(|_| format!("go list reported {} outside the repository", dir.display()))?;
    let text = rel
        .to_str()
        .ok_or_else(|| format!("{} isn't UTF-8", rel.display()))?
        .replace('\\', "/");
    Ok(if text.is_empty() { ".".into() } else { text })
}

impl Graph {
    fn target(&self, import: &str) -> Option<Target> {
        let p = self.packages.get(import)?;
        Some(Target::Go {
            module: p.module.clone(),
            dir: p.module_dir.clone(),
            package: import.to_string(),
        })
    }

    /// The packages whose tests a change to `seeds` needs, or why the run
    /// must be full: a folder `go list` didn't report imports one of them.
    fn dependents(&self, seeds: &BTreeSet<String>) -> Mapping {
        let mut closure: BTreeSet<String> = seeds.clone();
        let mut queue: VecDeque<String> = seeds.iter().cloned().collect();
        while let Some(p) = queue.pop_front() {
            for importer in self.importers.get(&p).into_iter().flatten() {
                if closure.insert(importer.clone()) {
                    queue.push_back(importer.clone());
                }
            }
        }
        if let Some(dir) = closure
            .iter()
            .find_map(|p| self.unlisted_importers.get(p).and_then(|d| d.first()))
        {
            return Mapping::Unaccounted(format!(
                "{dir} holds Go files that build only under other settings (a tag, another platform) and depend on this change"
            ));
        }
        let mut tested = closure.clone();
        for p in &closure {
            tested.extend(self.test_importers.get(p).into_iter().flatten().cloned());
        }
        Mapping::Selects(tested.iter().filter_map(|p| self.target(p)).collect())
    }

    /// The package owning a non-Go file, if any.
    fn owner_of_data(&self, path: &str) -> BTreeSet<String> {
        let mut owners: BTreeSet<String> = self.embedded.get(path).cloned().unwrap_or_default();
        // The module's top folder holds the build's and CI's own files
        // (a Makefile, a Dockerfile), which its package's tests rarely read.
        let dir = parent(path);
        if let Some(p) = self
            .by_dir
            .get(dir)
            .filter(|_| !self.module_dirs.contains(dir))
        {
            owners.insert(p.clone());
        }
        // Under a package's `testdata/`: the package holding that folder.
        let segments: Vec<&str> = path.split('/').collect();
        if let Some(i) = segments.iter().position(|s| *s == "testdata") {
            let dir = if i == 0 {
                ".".to_string()
            } else {
                segments[..i].join("/")
            };
            if let Some(p) = self.by_dir.get(&dir) {
                owners.insert(p.clone());
            }
        }
        owners
    }
}

impl Adapter for GoAdapter {
    fn kind(&self) -> MatchKind {
        MatchKind::Go
    }

    fn name(&self) -> &'static str {
        "go"
    }

    fn map(&mut self, path: &str, deleted: bool) -> Result<Mapping, String> {
        if self.module_of(path).is_none() {
            return Ok(Mapping::NotMine);
        }
        let is_go = path.ends_with(".go");
        let graph = self.graph()?;
        if deleted && graph.embed_dirs.iter().any(|dir| within(dir, path)) {
            return Ok(Mapping::Unaccounted(format!(
                "{path} was removed beneath a package with embeds; the head can't prove its former embed owners"
            )));
        }
        let in_testdata = path.split('/').any(|s| s == "testdata");
        if is_go && !in_testdata {
            let dir = parent(path);
            return Ok(match graph.by_dir.get(dir) {
                Some(p) => {
                    let mut owners = graph.embedded.get(path).cloned().unwrap_or_default();
                    owners.insert(p.clone());
                    graph.dependents(&owners)
                }
                None if deleted => Mapping::Removed(format!(
                    "no package builds {dir} at the head; a full run checks its former importers"
                )),
                None => Mapping::Unaccounted(format!(
                    "no package in {dir} includes this file on this platform (build constraints, or a folder Go ignores)"
                )),
            });
        }
        let owners = graph.owner_of_data(path);
        if owners.is_empty() {
            return Ok(Mapping::NotMine);
        }
        Ok(graph.dependents(&owners))
    }

    fn resolve(&mut self, names: &[String]) -> Result<BTreeSet<Target>, String> {
        let graph = self.graph()?;
        let mut out = BTreeSet::new();
        for name in names {
            let (dir, recursive) = match name.strip_suffix("/...") {
                Some(d) => (d, true),
                None if name == "./..." => (".", true),
                None => (name.as_str(), false),
            };
            let dir = match dir.strip_prefix("./") {
                Some(d) => d,
                None if dir == "." => ".",
                None => return Err(format!("`{name}` must start with `./`")),
            };
            let matched: Vec<&String> = graph
                .by_dir
                .iter()
                .filter(|(d, _)| *d == dir || (recursive && within(dir, d)))
                .map(|(_, p)| p)
                .collect();
            if matched.is_empty() {
                return Err(format!("no Go package matches `{name}`"));
            }
            out.extend(matched.into_iter().filter_map(|p| graph.target(p)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_imports_of_a_file_header() {
        let src = "// Copyright\n//go:build linux\n\n/* doc */ package p // x\n\nimport \"a\"\nimport b \"m/b\"\nimport (\n\t\"m/c\" // c\n\t_ \"m/d\"\n\t. `m/e`\n\t/* f */ f \"m/f\"\n)\n\nimport \"m/g\"; func X() { _ = \"not/an/import\" }\n";
        assert_eq!(
            parse_imports(src.as_bytes()).unwrap(),
            ["a", "m/b", "m/c", "m/d", "m/e", "m/f", "m/g"]
        );
        assert_eq!(parse_imports(b"package p\n").unwrap(), Vec::<String>::new());
        // A BOM, newlines after `package` and `import`, and escapes.
        let src = "\u{feff}package\np\nimport\n\"m/\\x61\"\nimport\n(\n\t\"m/\\\"q\"\n)\n";
        assert_eq!(parse_imports(src.as_bytes()).unwrap(), ["m/a", "m/\"q"]);
        assert!(never_builds(
            b"// Generated.\n\n//go:build ignore\n\npackage {{.Name}}\n"
        ));
        assert!(!never_builds(b"//go:build ignore || linux\npackage p\n"));
        assert!(!never_builds(b"package p\n//go:build ignore\n"));
        for bad in [
            &b"func X() {}"[..],
            b"package p\nimport (\n\"a\"\n",
            b"package p\nimport \"a\\q\"\n",
            b"\xff",
        ] {
            assert!(parse_imports(bad).is_err(), "{bad:?}");
        }
    }

    fn graph() -> Graph {
        // m/a <- m/b <- m/c (imports), m/d's tests import m/b, m/e imports
        // nothing. m/b embeds web/index.html.
        let mut g = Graph::default();
        for (p, dir) in [
            ("m/a", "a"),
            ("m/b", "b"),
            ("m/c", "c"),
            ("m/d", "d"),
            ("m/e", "e"),
        ] {
            g.packages.insert(
                p.into(),
                Package {
                    module: "m".into(),
                    module_dir: ".".into(),
                },
            );
            g.by_dir.insert(dir.into(), p.into());
        }
        g.importers
            .entry("m/a".into())
            .or_default()
            .insert("m/b".into());
        g.importers
            .entry("m/b".into())
            .or_default()
            .insert("m/c".into());
        g.test_importers
            .entry("m/b".into())
            .or_default()
            .insert("m/d".into());
        g.embedded
            .entry("b/web/index.html".into())
            .or_default()
            .insert("m/b".into());
        g
    }

    fn names(m: &Mapping) -> Vec<String> {
        match m {
            Mapping::Selects(t) => t.iter().map(Target::label).collect(),
            other => panic!("not a selection: {other:?}"),
        }
    }

    #[test]
    fn dependents_follow_imports_and_stop_at_test_imports() {
        let g = graph();
        assert_eq!(
            names(&g.dependents(&BTreeSet::from(["m/a".to_string()]))),
            ["go:m/a", "go:m/b", "go:m/c", "go:m/d"]
        );
        assert_eq!(
            names(&g.dependents(&BTreeSet::from(["m/d".to_string()]))),
            ["go:m/d"],
            "nothing imports d; its tests' imports don't make d a dependent of anything"
        );
        assert_eq!(
            names(&g.dependents(&BTreeSet::from(["m/e".to_string()]))),
            ["go:m/e"]
        );
    }

    #[test]
    fn data_files_belong_to_their_package_its_testdata_or_its_embeds() {
        let g = graph();
        assert_eq!(
            g.owner_of_data("a/schema.sql"),
            BTreeSet::from(["m/a".to_string()])
        );
        assert_eq!(
            g.owner_of_data("a/testdata/x/y.json"),
            BTreeSet::from(["m/a".to_string()])
        );
        assert_eq!(
            g.owner_of_data("b/web/index.html"),
            BTreeSet::from(["m/b".to_string()])
        );
        assert!(g.owner_of_data("migrations/001.sql").is_empty());
        assert!(
            g.owner_of_data("a/docs/notes.md").is_empty(),
            "a subfolder that isn't a package"
        );
    }

    #[test]
    fn module_lines() {
        let dir = std::env::temp_dir().join(format!("mdkg-affected-gomod-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (text, want) in [
            ("module example.com/m\n\ngo 1.22\n", Some("example.com/m")),
            (
                "// comment\nmodule \"example.com/q\" // trailing\n",
                Some("example.com/q"),
            ),
            ("modulex example.com/m\n", None),
            ("go 1.22\n", None),
        ] {
            std::fs::write(dir.join("go.mod"), text).unwrap();
            assert_eq!(module_path(&dir).ok().as_deref(), want, "{text:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovery_skips_modules_go_ignores() {
        let files: Vec<String> = [
            "go.mod",
            "tools/go.mod",
            "x/testdata/go.mod",
            "vendor/y/go.mod",
            "_old/go.mod",
            "Testdata/GO.MOD",
            "Vendor/go.mod",
            "testdata/GO.MOD",
        ]
        .map(String::from)
        .to_vec();
        let a = GoAdapter::discover(Path::new("/r"), Path::new("/c"), &files).unwrap();
        let dirs: Vec<&str> = a.modules.iter().map(|m| m.dir.as_str()).collect();
        assert_eq!(dirs, ["Testdata", "Vendor", "tools", "."]);
        assert!(
            GoAdapter::discover(
                Path::new("/r"),
                Path::new("/c"),
                &["Cargo.toml".to_string()]
            )
            .is_none()
        );
    }
}
