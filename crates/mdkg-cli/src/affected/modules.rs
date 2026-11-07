//! Rust module trees, read from the head's snapshot: the `.rs` files one
//! target's crate root reaches through `mod` declarations (with or without
//! `#[path]`) and the `include!` family of macros.
//!
//! It reads tokens, not a full syntax tree, and errs towards reaching more
//! files: a `mod` under `#[cfg]` counts, a `mod` in a `macro_rules!` body
//! counts where the macro is defined. What it can't follow soundly (a module a macro
//! names, a `cfg_attr` path, an include with a computed path, a `mod` whose
//! file is missing, an unreadable or oversized file) makes the tree
//! uncertain: it may then reach any file in the repository, since a
//! computed path can climb out of its root's folder.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The files one target's module tree reaches.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tree {
    /// Repository-relative file to the files that declare or include it
    /// (none for the root).
    pub files: BTreeMap<String, BTreeSet<String>>,
    /// Why the tree can't be read soundly, when it can't.
    pub uncertain: Option<String>,
}

impl Tree {
    /// Whether the tree reaches `path`. Its files are exact tracked
    /// spellings (see [`scan`]), so this compares exactly.
    pub fn reaches(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }
}

const MAX_FILES: usize = 20_000;
const MAX_BYTES: u64 = 8 << 20;

/// How a module file owns the folder for its `mod` children: a mod-rs
/// file (a crate root or a `mod.rs`) its own folder, any other file the
/// folder named after it. A file a `#[path]` names owns its own folder,
/// as rustc reads it (`DirOwnership::Owned`), whatever its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    ModRs,
    Named,
}

/// Reads the tree of the crate root `root` (repository-relative) from the
/// snapshot at `top`, whose tracked paths are `tracked`.
///
/// A file counts as present only when its path is exactly a tracked one,
/// never by asking the filesystem: a case-insensitive or
/// Unicode-normalizing volume opens other spellings of a tracked file, and
/// a file one host can't open may compile on another. Any named path that
/// isn't exactly tracked makes the tree uncertain, so the result is the
/// same on every host.
pub fn scan(top: &Path, root: &str, tracked: &BTreeSet<String>) -> Tree {
    let mut tree = Tree::default();
    tree.files.insert(root.to_string(), BTreeSet::new());
    if !tracked.contains(root) {
        tree.uncertain = Some(format!("{root} isn't a tracked path with that spelling"));
    }
    let mut queue = vec![(root.to_string(), Kind::ModRs, false)];
    let mut seen = BTreeSet::new();
    while let Some((file, kind, included)) = queue.pop() {
        if !seen.insert((file.clone(), kind, included)) {
            continue;
        }
        if seen.len() > MAX_FILES {
            tree.uncertain = Some(format!("more than {MAX_FILES} module files"));
            break;
        }
        let text = match read(top, &file) {
            Ok(t) => t,
            Err(e) => {
                tree.uncertain.get_or_insert(e);
                continue;
            }
        };
        let found = items(&text);
        if let Some(why) = found.uncertain {
            tree.uncertain.get_or_insert(format!("{file}: {why}"));
        }
        for decl in found.mods {
            if included {
                // A `mod` in an `include!`d file resolves against the
                // including module, which this reader doesn't track.
                tree.uncertain.get_or_insert(format!(
                    "{file} is included with include! and declares module {}",
                    decl.name
                ));
                continue;
            }
            let candidates = decl_candidates(&file, kind, &decl);
            let mut any = false;
            for (path, child_kind) in candidates {
                let Some(path) = path else {
                    tree.uncertain.get_or_insert(format!(
                        "{file}: module {} names a path outside the repository",
                        decl.name
                    ));
                    continue;
                };
                if tracked.contains(&path) {
                    any = true;
                    tree.files
                        .entry(path.clone())
                        .or_default()
                        .insert(file.clone());
                    queue.push((path, child_kind, false));
                }
            }
            if !any {
                tree.uncertain.get_or_insert(format!(
                    "{file}: no file at the head holds module {}",
                    decl.name
                ));
            }
        }
        for inc in found.includes {
            let Some(path) = join(dir(&file), &inc.path) else {
                tree.uncertain.get_or_insert(format!(
                    "{file}: an include names a path outside the repository"
                ));
                continue;
            };
            if !tracked.contains(&path) {
                tree.uncertain.get_or_insert(format!(
                    "{file}: an include names {path}, which isn't a tracked path with that spelling"
                ));
            }
            tree.files
                .entry(path.clone())
                .or_default()
                .insert(file.clone());
            // `include!` takes code from a file of any name.
            if inc.code && tracked.contains(&path) {
                queue.push((path, kind, true));
            }
        }
    }
    tree
}

fn read(top: &Path, file: &str) -> Result<String, String> {
    let path = top.join(file);
    let meta = std::fs::metadata(&path).map_err(|e| format!("can't read {file}: {e}"))?;
    if meta.len() > MAX_BYTES {
        return Err(format!("{file} is over {MAX_BYTES} bytes"));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("can't read {file}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{file} isn't UTF-8"))
}

fn dir(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(d, _)| d)
}

fn stem(file: &str) -> &str {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.strip_suffix(".rs").unwrap_or(name)
}

/// `base` joined with a relative `rel`, `..` resolved lexically (the
/// snapshot has no symlinks); `None` when it leaves the repository or is
/// absolute.
fn join(base: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') || rel.starts_with('\\') || rel.contains(':') {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in rel.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Where a declaration's file can be, and how that file owns its folder.
fn decl_candidates(file: &str, kind: Kind, decl: &ModDecl) -> Vec<(Option<String>, Kind)> {
    let kinds: &[Kind] = match kind {
        Kind::ModRs => &[Kind::ModRs],
        Kind::Named => &[Kind::Named],
    };
    let mut bases = Vec::new();
    for k in kinds {
        let mut base = dir(file).to_string();
        // A mod.rs owns its folder whatever named it. A path attribute
        // outside any inline module is relative to the file's own folder.
        let named = *k == Kind::Named
            && stem(file) != "mod"
            && !(decl.path.is_some() && decl.inline.is_empty());
        if named {
            base = push(&base, stem(file));
        }
        for m in &decl.inline {
            base = push(&base, m);
        }
        bases.push(base);
    }
    let mut out = Vec::new();
    for base in bases {
        match &decl.path {
            Some(p) => out.push((join(&base, p), Kind::ModRs)),
            None => {
                out.push((join(&base, &format!("{}.rs", decl.name)), Kind::Named));
                out.push((join(&base, &format!("{}/mod.rs", decl.name)), Kind::ModRs));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn push(base: &str, part: &str) -> String {
    if base.is_empty() {
        part.to_string()
    } else {
        format!("{base}/{part}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModDecl {
    name: String,
    /// `#[path]`'s value, when it has one.
    path: Option<String>,
    /// The inline modules (`mod a { ... }`) it sits in, outermost first.
    inline: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Include {
    path: String,
    /// `include!` (Rust code) rather than `include_str!`/`include_bytes!`.
    code: bool,
}

#[derive(Debug, Default)]
struct Items {
    mods: Vec<ModDecl>,
    includes: Vec<Include>,
    uncertain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    /// A string literal's value; `None` when it has escapes this reader
    /// doesn't decode.
    Str(Option<String>),
    Punct(char),
    Other,
}

/// The out-of-line `mod` declarations and includes in one file.
fn items(text: &str) -> Items {
    let toks = match lex(text) {
        Ok(t) => t,
        Err(why) => {
            return Items {
                uncertain: Some(why),
                ..Items::default()
            };
        }
    };
    let mut out = Items::default();
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut path: Option<Option<String>> = None;
    let mut i = 0;
    let at = |i: usize| toks.get(i);
    while i < toks.len() {
        match &toks[i] {
            Tok::Punct('#') => {
                let mut j = i + 1;
                if at(j) == Some(&Tok::Punct('!')) {
                    j += 1;
                }
                if at(j) != Some(&Tok::Punct('[')) {
                    i += 1;
                    continue;
                }
                let start = j + 1;
                let mut depth = 0usize;
                let mut end = start;
                while end < toks.len() {
                    match toks[end] {
                        Tok::Punct('[') => depth += 1,
                        Tok::Punct(']') if depth == 0 => break,
                        Tok::Punct(']') => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                let attr = &toks[start..end.min(toks.len())];
                match attr.first() {
                    Some(Tok::Ident(name)) if name == "path" => match attr {
                        [_, Tok::Punct('='), Tok::Str(value)] => {
                            if value.is_none() {
                                out.uncertain.get_or_insert(
                                    "a #[path] value has escapes this reader doesn't decode".into(),
                                );
                            }
                            path = Some(value.clone());
                        }
                        _ => {
                            out.uncertain
                                .get_or_insert("a #[path] attribute it can't read".into());
                        }
                    },
                    Some(Tok::Ident(name))
                        if name == "cfg_attr"
                            && attr.iter().any(|t| *t == Tok::Ident("path".into())) =>
                    {
                        out.uncertain
                            .get_or_insert("a #[cfg_attr] that may set a module path".into());
                    }
                    _ => {}
                }
                // Read on inside it too: `#[doc = include_str!(..)]`.
                i = start;
                continue;
            }
            Tok::Ident(kw) if kw == "mod" => {
                match (at(i + 1), at(i + 2)) {
                    (Some(Tok::Ident(name)), Some(Tok::Punct(';'))) => {
                        match path.take() {
                            Some(None) => {}
                            p => out.mods.push(ModDecl {
                                name: name.clone(),
                                path: p.flatten(),
                                inline: stack.iter().flatten().cloned().collect(),
                            }),
                        }
                        if stack.iter().any(Option::is_none) {
                            // A `mod` inside a function or other block
                            // resolves like one in its module; one inside a
                            // macro body may be expanded anywhere.
                            out.uncertain.get_or_insert(format!(
                                "module {name} is declared inside a block or macro"
                            ));
                        }
                        i += 3;
                        continue;
                    }
                    (Some(Tok::Ident(name)), Some(Tok::Punct('{'))) => {
                        if path.take().is_some() {
                            out.uncertain
                                .get_or_insert(format!("inline module {name} has a #[path]"));
                        }
                        stack.push(Some(name.clone()));
                        i += 3;
                        continue;
                    }
                    (Some(Tok::Ident(_)), _) => {}
                    _ => {
                        out.uncertain
                            .get_or_insert("a module whose name a macro supplies".into());
                    }
                }
            }
            Tok::Ident(mac)
                if matches!(mac.as_str(), "include" | "include_str" | "include_bytes")
                    && at(i + 1) == Some(&Tok::Punct('!')) =>
            {
                match (at(i + 2), at(i + 3), at(i + 4)) {
                    (
                        Some(Tok::Punct('(' | '[' | '{')),
                        Some(Tok::Str(Some(value))),
                        Some(Tok::Punct(')' | ']' | '}' | ',')),
                    ) => out.includes.push(Include {
                        path: value.clone(),
                        code: mac == "include",
                    }),
                    _ => {
                        out.uncertain
                            .get_or_insert(format!("{mac}! with a path it can't read"));
                    }
                }
                i += 2;
                continue;
            }
            Tok::Punct('{') => {
                stack.push(None);
                path = None;
            }
            Tok::Punct('}') => {
                stack.pop();
                path = None;
            }
            Tok::Punct(';') => path = None,
            _ => {}
        }
        i += 1;
    }
    out
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Rust tokens enough to find items: comments dropped, literals kept as
/// one token each.
fn lex(text: &str) -> Result<Vec<Tok>, String> {
    let s: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let n = s.len();
    while i < n {
        let c = s[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && s.get(i + 1) == Some(&'/') {
            while i < n && s[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && s.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            loop {
                if i >= n {
                    return Err("an unclosed block comment".into());
                }
                if s[i] == '/' && s.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if s[i] == '*' && s.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == '"' {
            let (tok, next) = quoted(&s, i + 1)?;
            out.push(tok);
            i = next;
        } else if is_ident_start(c) {
            // Raw strings and prefixed literals: r"", r#""#, b"", br"",
            // c"", cr"", b''.
            let mut j = i;
            while j < n && is_ident(s[j]) {
                j += 1;
            }
            let word: String = s[i..j].iter().collect();
            let raw = matches!(word.as_str(), "r" | "br" | "cr");
            if raw && j < n && (s[j] == '"' || s[j] == '#') {
                let mut hashes = 0;
                let mut k = j;
                while k < n && s[k] == '#' {
                    hashes += 1;
                    k += 1;
                }
                if k < n && s[k] == '"' {
                    let (value, next) = raw_string(&s, k + 1, hashes)?;
                    out.push(Tok::Str(Some(value)));
                    i = next;
                    continue;
                }
                if word == "r" && hashes == 1 && k < n && is_ident_start(s[k]) {
                    // A raw identifier.
                    let mut e = k;
                    while e < n && is_ident(s[e]) {
                        e += 1;
                    }
                    out.push(Tok::Ident(s[k..e].iter().collect()));
                    i = e;
                    continue;
                }
            }
            if matches!(word.as_str(), "b" | "c") && j < n && s[j] == '"' {
                let (tok, next) = quoted(&s, j + 1)?;
                out.push(tok);
                i = next;
                continue;
            }
            if word == "b" && j < n && s[j] == '\'' {
                i = char_literal(&s, j).unwrap_or(j + 1);
                out.push(Tok::Other);
                continue;
            }
            out.push(Tok::Ident(word));
            i = j;
        } else if c.is_ascii_digit() {
            while i < n
                && (is_ident(s[i])
                    || s[i] == '.' && s.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
            {
                i += 1;
            }
            out.push(Tok::Other);
        } else if c == '\'' {
            match char_literal(&s, i) {
                Some(next) => i = next,
                // A lifetime or label.
                None => {
                    i += 1;
                    while i < n && is_ident(s[i]) {
                        i += 1;
                    }
                }
            }
            out.push(Tok::Other);
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    Ok(out)
}

/// A string literal's body from just past its opening quote: the token and
/// the index past its closing quote.
fn quoted(s: &[char], mut i: usize) -> Result<(Tok, usize), String> {
    let mut value = String::new();
    let mut plain = true;
    while i < s.len() {
        match s[i] {
            '"' => {
                return Ok((Tok::Str(plain.then_some(value)), i + 1));
            }
            '\\' => {
                plain = false;
                i += 2;
            }
            c => {
                value.push(c);
                i += 1;
            }
        }
    }
    Err("an unclosed string literal".into())
}

fn raw_string(s: &[char], mut i: usize, hashes: usize) -> Result<(String, usize), String> {
    let mut value = String::new();
    while i < s.len() {
        if s[i] == '"' && (0..hashes).all(|h| s.get(i + 1 + h) == Some(&'#')) {
            return Ok((value, i + 1 + hashes));
        }
        value.push(s[i]);
        i += 1;
    }
    Err("an unclosed raw string literal".into())
}

/// The index past a character literal starting at `i` (its opening
/// quote), or `None` when it's a lifetime or label.
fn char_literal(s: &[char], i: usize) -> Option<usize> {
    match (s.get(i + 1), s.get(i + 2)) {
        (Some('\\'), _) => {
            let mut j = i + 2;
            // Past the escaped character, then to the closing quote.
            j += 1;
            while j < s.len() && s[j] != '\'' {
                if s[j] == '\n' {
                    return None;
                }
                j += 1;
            }
            (j < s.len()).then_some(j + 1)
        }
        (Some(_), Some('\'')) => Some(i + 3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decls(text: &str) -> Vec<(String, Option<String>, Vec<String>)> {
        items(text)
            .mods
            .into_iter()
            .map(|d| (d.name, d.path, d.inline))
            .collect()
    }

    #[test]
    fn finds_declarations_past_comments_strings_and_lifetimes() {
        let text = r##"
            // mod commented;
            /* mod blocked; /* nested */ mod still_blocked; */
            const S: &str = "mod quoted;";
            const R: &str = r#"mod raw; "# ;
            fn f<'a>(x: &'a str) -> char { let _ = '"'; let _ = '\''; 'x' }
            pub mod real;
            pub(crate) mod r#type;
            #[cfg(test)]
            mod tests;
            mod inline { mod inner; }
            #[path = "other/place.rs"]
            mod placed;
        "##;
        assert_eq!(
            decls(text),
            [
                ("real".into(), None, vec![]),
                ("type".into(), None, vec![]),
                ("tests".into(), None, vec![]),
                ("inner".into(), None, vec!["inline".into()]),
                ("placed".into(), Some("other/place.rs".into()), vec![]),
            ]
        );
        assert_eq!(items(text).uncertain, None);
    }

    #[test]
    fn what_it_cant_follow_is_uncertain() {
        for text in [
            "macro_rules! m { ($n:ident) => { mod $n; } }",
            "#[cfg_attr(unix, path = \"u.rs\")] mod x;",
            "const X: &str = include_str!(concat!(env!(\"OUT_DIR\"), \"/x\"));",
            "fn f() { mod hidden; }",
            "#[path = \"a\\\\b.rs\"] mod x;",
            "/* unclosed",
        ] {
            assert!(items(text).uncertain.is_some(), "{text}");
        }
    }

    #[test]
    fn includes_are_read_with_their_kind() {
        let found = items(
            "const A: &str = include_str!(\"data.txt\"); include!(\"gen.rs\"); static B: &[u8] = std::include_bytes!(\"b.bin\");",
        );
        assert_eq!(
            found.includes,
            [
                Include {
                    path: "data.txt".into(),
                    code: false
                },
                Include {
                    path: "gen.rs".into(),
                    code: true
                },
                Include {
                    path: "b.bin".into(),
                    code: false
                },
            ]
        );
    }

    #[test]
    fn declarations_resolve_like_rustc() {
        let d = |name: &str, path: Option<&str>, inline: &[&str]| ModDecl {
            name: name.into(),
            path: path.map(Into::into),
            inline: inline.iter().map(|s| s.to_string()).collect(),
        };
        let paths = |file: &str, kind: Kind, decl: &ModDecl| -> Vec<String> {
            decl_candidates(file, kind, decl)
                .into_iter()
                .filter_map(|(p, _)| p)
                .collect()
        };
        // A crate root and a mod.rs own their folder.
        assert_eq!(
            paths("t/x.rs", Kind::ModRs, &d("common", None, &[])),
            ["t/common.rs", "t/common/mod.rs"]
        );
        assert_eq!(
            paths("t/common/mod.rs", Kind::ModRs, &d("temp", None, &[])),
            ["t/common/temp.rs", "t/common/temp/mod.rs"]
        );
        // Another file owns the folder named after it.
        assert_eq!(
            paths("s/msg.rs", Kind::Named, &d("wait", None, &[])),
            ["s/msg/wait.rs", "s/msg/wait/mod.rs"]
        );
        // Inline modules add folders.
        assert_eq!(
            paths("s/msg.rs", Kind::Named, &d("c", None, &["a", "b"])),
            ["s/msg/a/b/c.rs", "s/msg/a/b/c/mod.rs"]
        );
        // #[path] outside inline modules: the file's own folder.
        assert_eq!(
            paths(
                "s/coord.rs",
                Kind::Named,
                &d("g", Some("graph_git.rs"), &[])
            ),
            ["s/graph_git.rs"]
        );
        assert_eq!(
            paths(
                "t/common/mod.rs",
                Kind::ModRs,
                &d("s", Some("../../c/s.rs"), &[])
            ),
            ["c/s.rs"]
        );
        // #[path] inside an inline module of a named file.
        assert_eq!(
            paths("s/msg.rs", Kind::Named, &d("g", Some("g.rs"), &["a"])),
            ["s/msg/a/g.rs"]
        );
        // A file a #[path] named owns its own folder.
        assert_eq!(
            decl_candidates("s/x.rs", Kind::Named, &d("g", Some("graph_git.rs"), &[]))[0].1,
            Kind::ModRs
        );
        // Leaving the repository can't be followed.
        assert_eq!(
            decl_candidates("t/x.rs", Kind::ModRs, &d("o", Some("../../o.rs"), &[]))[0].0,
            None
        );
    }

    #[test]
    fn a_tree_follows_files_and_marks_missing_ones() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-modules-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let write = |p: &str, text: &str| {
            let path = dir.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "tests/a.rs",
            "mod common;\n#[path = \"common/temp.rs\"] mod temp;\n",
        );
        write(
            "tests/common/mod.rs",
            "pub mod template;\nconst D: &str = include_str!(\"data.json\");\n",
        );
        write("tests/common/template.rs", "");
        write("tests/common/temp.rs", "");
        write("tests/b.rs", "mod gone;\n");
        write("tests/common/data.json", "{}");
        let tracked: BTreeSet<String> = [
            "tests/a.rs",
            "tests/b.rs",
            "tests/common/mod.rs",
            "tests/common/template.rs",
            "tests/common/temp.rs",
            "tests/common/data.json",
        ]
        .map(String::from)
        .into();
        let a = scan(&dir, "tests/a.rs", &tracked);
        assert_eq!(a.uncertain, None);
        assert_eq!(
            a.files.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "tests/a.rs",
                "tests/common/data.json",
                "tests/common/mod.rs",
                "tests/common/temp.rs",
                "tests/common/template.rs"
            ]
        );
        let b = scan(&dir, "tests/b.rs", &tracked);
        assert!(b.uncertain.as_deref().unwrap().contains("gone"), "{b:?}");
        // A file that opens but isn't tracked with that spelling (another
        // case, another Unicode form, or not tracked at all) is uncertain.
        write("tests/c.rs", "#[path = \"common/TEMP.rs\"] mod t;\n");
        write(
            "tests/d.rs",
            "include!(\"common/temp.rs\");\ninclude!(\"common/x.inc\");\n",
        );
        write("tests/common/x.inc", "");
        let mut more = tracked.clone();
        more.insert("tests/c.rs".into());
        more.insert("tests/d.rs".into());
        let c = scan(&dir, "tests/c.rs", &more);
        assert!(
            c.uncertain.as_deref().unwrap().contains("module t"),
            "{c:?}"
        );
        let d = scan(&dir, "tests/d.rs", &more);
        assert!(d.uncertain.as_deref().unwrap().contains("x.inc"), "{d:?}");
        assert!(scan(&dir, "tests/c.rs", &tracked).uncertain.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
