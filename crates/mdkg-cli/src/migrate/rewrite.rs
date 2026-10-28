//! Text rewrites: extension keys, `capability search` gates, legacy id
//! tokens, and the body sections that move to the log.
//!
//! Each works line by line on the original text and changes only what it
//! must, so a migrated file's diff shows exactly the rewrite.

use super::classify::{SectionKind, section_kind};

/// The namespaced `x-<namespace>.<field>` key for a 0.6.0-era
/// `x_<namespace>_<field>` key, splitting at the first `_` after `x_`
/// (lane 1, 2026-10-24). Returns the key and whether the split was a guess
/// (the field holds another `_`, so the namespace could have been longer);
/// `Err` when there's no valid split (`x_owner`, `x__a`, `x_1co_a`): the
/// key stays as written, with a warning.
pub fn extension_key(key: &str) -> Option<Result<(String, bool), ()>> {
    let rest = key.strip_prefix("x_")?;
    // Namespaces are `[a-z][a-z0-9-]*`; fields `[a-z][a-z0-9_]*`.
    let ns_ok = |s: &str| {
        s.starts_with(|c: char| c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    let field_ok = |s: &str| {
        s.starts_with(|c: char| c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    Some(match rest.split_once('_') {
        Some((ns, field)) if ns_ok(ns) && field_ok(field) => {
            Ok((format!("x-{ns}.{field}"), field.contains('_')))
        }
        _ => Err(()),
    })
}

/// Options `mdkg search` has no equivalent for; dropped from a rewritten
/// gate, with the value they take.
const DROPPED_OPTIONS: &[(&str, bool)] = &[
    ("--kind", true),
    ("--visibility", true),
    ("--no-cache", false),
    ("--no-reindex", false),
];

/// Rewrites every `capability search` invocation in `text` to `search`,
/// dropping the options `search` lacks (C1). Returns the new text, or
/// `None` when nothing changed, and whether an option was dropped.
pub fn capability_search(text: &str) -> Option<(String, bool)> {
    capability_search_as(text, mdkg_format::program::name())
}

/// [`capability_search`] where the binary is also called `bin` (the name
/// this build runs under, such as `mdkg-next` beside 0.6.0's `mdkg`).
fn capability_search_as(text: &str, bin: &str) -> Option<(String, bool)> {
    const NEEDLE: &str = "capability search";
    let own = format!("{bin} ");
    if !text.contains(NEEDLE) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut dropped = false;
    let mut rest = text;
    while let Some(i) = rest.find(NEEDLE) {
        let before = &rest[..i];
        // Only the command, `mdkg capability search` (or under the name
        // this build runs as, or a path ending in `/mdkg`), not prose about
        // capability searches.
        let is_command = before.ends_with("mdkg ") || before.ends_with(&own);
        let after = &rest[i + NEEDLE.len()..];
        let ends_word = !after.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '-');
        if !(is_command && ends_word) {
            out.push_str(&rest[..i + NEEDLE.len()]);
            rest = after;
            continue;
        }
        out.push_str(before);
        out.push_str("search");
        // The invocation runs to the end of the line or a closing backtick.
        let end = after.find(['\n', '`', ']']).unwrap_or(after.len());
        let (args, tail) = after.split_at(end);
        let (args, d) = drop_options(args);
        dropped |= d;
        out.push_str(&args);
        rest = tail;
    }
    out.push_str(rest);
    (out != text).then_some((out, dropped))
}

/// Removes [`DROPPED_OPTIONS`] from a command's arguments, keeping their
/// spacing otherwise.
fn drop_options(args: &str) -> (String, bool) {
    let words: Vec<&str> = args.split(' ').collect();
    let mut kept = Vec::with_capacity(words.len());
    let mut dropped = false;
    let mut skip_value = false;
    for w in words {
        if skip_value {
            skip_value = false;
            if !w.is_empty() {
                continue;
            }
        }
        let name = w.split_once('=').map_or(w, |(n, _)| n);
        if let Some((_, takes)) = DROPPED_OPTIONS.iter().find(|(o, _)| *o == name) {
            dropped = true;
            skip_value = *takes && !w.contains('=');
            continue;
        }
        kept.push(w);
    }
    (kept.join(" "), dropped)
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A legacy id token found in text: `[qualifier:]prefix-number`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'a> {
    /// The whole token as written.
    pub text: &'a str,
    /// The workspace or subgraph alias before `:`, if any.
    pub qualifier: Option<&'a str>,
    /// `prefix-number`.
    pub id: &'a str,
    /// Where the token starts in the text searched.
    pub start: usize,
}

/// Matches `[a-z]+-[0-9]+` at `s`'s start; returns its length.
fn canonical_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_lowercase() {
        i += 1;
    }
    if i == 0 || b.get(i) != Some(&b'-') {
        return None;
    }
    let digits = i + 1;
    let mut j = digits;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    (j > digits).then_some(j)
}

/// Rewrites each legacy id token in `text` that `map` knows. A token stands
/// alone: not inside a word, a path or a URL (no letter, digit, `_`, `-`,
/// `.`, `/` or `:` before it, unless the `:` follows its qualifier), and
/// not followed by a letter, digit, `_`, or `-` and a letter or digit
/// (`<type>-<number>-<slug>.md` is a file name). Returns the new text and
/// the tokens left alone because `map` didn't know them.
pub fn rewrite_tokens<'a>(
    text: &'a str,
    mut map: impl FnMut(&Token<'_>) -> Option<String>,
) -> (String, Vec<Token<'a>>) {
    let mut out = String::with_capacity(text.len());
    let mut unknown = Vec::new();
    let bytes = text.as_bytes();
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let starts_word = c.is_ascii_lowercase()
            && (i == 0 || {
                let p = text[..i].chars().next_back().unwrap_or(' ');
                !(is_id_char(p) || matches!(p, '-' | '.' | '/' | ':' | '@' | '#'))
            });
        if !starts_word {
            i += 1;
            continue;
        }
        // An optional `alias:` qualifier.
        let word_end = i + text[i..]
            .find(|ch: char| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'))
            .unwrap_or(text.len() - i);
        let (qualifier, id_start) = if bytes.get(word_end) == Some(&b':')
            && canonical_len(&text[word_end + 1..]).is_some()
        {
            (Some(&text[i..word_end]), word_end + 1)
        } else {
            (None, i)
        };
        let Some(len) = canonical_len(&text[id_start..]) else {
            i = word_end.max(i + 1);
            continue;
        };
        let end = id_start + len;
        let next = text[end..].chars().next();
        let next2 = text[end..].chars().nth(1);
        let joined = next.is_some_and(is_id_char)
            || (next == Some('-') && next2.is_some_and(|c| c.is_ascii_alphanumeric()));
        if joined {
            i = end;
            continue;
        }
        let token = Token {
            text: &text[i..end],
            qualifier,
            id: &text[id_start..end],
            start: i,
        };
        match map(&token) {
            Some(new) => {
                out.push_str(&text[copied..i]);
                out.push_str(&new);
                copied = end;
            }
            None => unknown.push(token),
        }
        i = end;
    }
    out.push_str(&text[copied..]);
    (out, unknown)
}

/// How a body mentions a node it names, by where the mention sits.
fn prose_mention(segment: &str, t: &Token<'_>, new: &str) -> Option<String> {
    let before = &segment[..t.start];
    let after = &segment[t.start + t.text.len()..];
    // A link's target: the id alone.
    if before.ends_with("](") {
        return Some(new.to_string());
    }
    // A link's text: left as written (its target says where it goes).
    let open = before.rfind('[');
    let close = before.rfind(']');
    let in_brackets = open.is_some_and(|o| close.is_none_or(|c| c < o));
    if in_brackets {
        let next_open = after.find('[').unwrap_or(after.len());
        if after[..next_open].contains("](") {
            return None;
        }
    }
    Some(format!("[{}]({new})", t.text))
}

/// Where each link target in `text` starts, as byte offsets: the format's
/// link grammar (section 5.1), by the validator's own reading, which
/// skips code and returns slices of `text`.
pub fn link_targets(text: &str) -> Vec<usize> {
    mdkg_graph::validate::body_links(text)
        .into_iter()
        .map(|t| t.as_ptr() as usize - text.as_ptr() as usize)
        .collect()
}

/// Rewrites each identity v2 ref in `text`, `mdkg://<uuid>/<uuid>`
/// standing alone, to what `map` gives for its graph, node and written
/// form; a ref `map` doesn't know stays as written. Standing alone means
/// judged by its whole word (the run of text between spaces): the ref
/// opens the word, after nothing but `([{<"'`*|`, or it starts at one of
/// `targets`, the byte offsets in `text` where a link's target starts by
/// the format's link grammar ([`link_targets`]), so a ref inside another
/// URL or a path
/// (`?items=a,mdkg://…`, `a/mdkg://…`, a `](` inside a link's target)
/// stays; and nothing that would
/// continue it follows (a letter, digit, `_`, `-`, `/`, `?`, `#`, `=`,
/// `&`, `@`, `%`, `+`, `~`, or `.` before a letter or digit).
pub fn identity_refs(
    text: &str,
    targets: &[usize],
    mut map: impl FnMut(&str, &str, &str) -> Option<String>,
) -> String {
    const SCHEME: &str = "mdkg://";
    // `<uuid>/<uuid>`: 36 + 1 + 36 bytes.
    const LEN: usize = SCHEME.len() + 73;
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = text[from..].find(SCHEME) {
        let at = from + found;
        from = at + SCHEME.len();
        let word_start = text[..at]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        let before = &text[word_start..at];
        let starts_word = before.chars().all(|c| "([{<\"'`*|".contains(c)) || targets.contains(&at);
        let ends_word = text.get(at + LEN..).is_none_or(|t| {
            let mut next = t.chars();
            match next.next() {
                None => true,
                Some('.') => !next.next().is_some_and(|c| c.is_alphanumeric()),
                Some(c) => !(is_id_char(c) || "-/?#=&@%+~".contains(c)),
            }
        });
        let Some(candidate) = text.get(at..at + LEN).filter(|_| starts_word && ends_word) else {
            continue;
        };
        let Some(r) = mdkg_format::ids::parse_identity_ref(candidate) else {
            continue;
        };
        if let Some(new) = map(&r.graph_id, &r.node_id, candidate) {
            out.push_str(&text[copied..at]);
            out.push_str(&new);
            copied = at + LEN;
            from = copied;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// What a mention in a body becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mention {
    /// A node of this graph (or another graph's, by `mdkg://` ref): a link
    /// in prose, the id in code.
    Node(String),
    /// A node only another branch holds: a link would name a node this
    /// branch lacks, so prose keeps the legacy id (handles resolve it) and
    /// code gets the id.
    CodeOnly(String),
    /// Known, and left as written (a node that left git).
    Leave,
    /// Nothing known.
    Unknown,
}

/// Rewrites the ids a node body mentions: in prose as a markdown link
/// whose text is the mention as written (`[<legacy-id>](<full-id>)`), inside
/// inline code and fenced blocks as the full id. `resolve` gives a
/// token's new id. Returns the body and the tokens `resolve` didn't know.
pub fn rewrite_body(
    body: &str,
    mut resolve: impl FnMut(&Token<'_>) -> Mention,
    mut resolve_identity: impl FnMut(&str, &str) -> Mention,
) -> (String, Vec<String>) {
    // An identity v2 ref (`mdkg://<graph>/<uuid>`) becomes the ref it
    // names, in prose and code alike (a link stays a link), except one
    // only another branch holds, which prose keeps as written.
    let mut identity = |text: &str, targets: &[usize], code: bool, unknown: &mut Vec<String>| {
        identity_refs(
            text,
            targets,
            |graph, uuid, written| match resolve_identity(graph, uuid) {
                Mention::Node(new) => Some(new),
                Mention::CodeOnly(new) if code => Some(new),
                Mention::CodeOnly(_) | Mention::Leave => None,
                Mention::Unknown => {
                    unknown.push(written.to_string());
                    None
                }
            },
        )
    };
    let mut out = String::with_capacity(body.len() + body.len() / 8);
    let mut unknown = Vec::new();
    let mut fence = Fence::default();
    for line in body.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        let was_open = fence.0.is_some();
        if fence.code(bare) && !was_open {
            // The opening line: its info string isn't code to rewrite.
            out.push_str(line);
            continue;
        }
        // Identity refs first, on the text as written (the offsets below
        // are into it), then legacy tokens, which never match the ids the
        // first pass writes (a UUID continues past its initial digits).
        if was_open {
            let line = identity(line, &[], true, &mut unknown);
            let (o, _) = rewrite_tokens(&line, |t| match resolve(t) {
                Mention::Node(new) | Mention::CodeOnly(new) => Some(new),
                Mention::Leave => None,
                Mention::Unknown => {
                    unknown.push(t.text.to_string());
                    None
                }
            });
            out.push_str(&o);
            continue;
        }
        // Link targets by the whole line, so a link whose title holds a
        // code span still counts; each segment gets its own offsets.
        let targets = link_targets(line);
        for (segment, code) in code_spans(line) {
            let start = segment.as_ptr() as usize - line.as_ptr() as usize;
            let mine: Vec<usize> = targets
                .iter()
                .filter_map(|t| t.checked_sub(start))
                .filter(|t| *t < segment.len())
                .collect();
            let segment = identity(segment, &mine, code, &mut unknown);
            let (o, _) = rewrite_tokens(&segment, |t| match resolve(t) {
                Mention::Node(new) if code => Some(new),
                Mention::Node(new) => prose_mention(&segment, t, &new),
                Mention::CodeOnly(new) if code => Some(new),
                Mention::CodeOnly(_) | Mention::Leave => None,
                Mention::Unknown => {
                    unknown.push(t.text.to_string());
                    None
                }
            });
            out.push_str(&o);
        }
    }
    (out, unknown)
}

/// A body section that moves to the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    /// The heading line, without `# `.
    pub heading: String,
    /// The section's text after the heading line, as written.
    pub text: String,
}

/// Fenced code as the 0.7 spec reads it (section 5.1), so the rewrite and
/// `validate` agree on what's code: a line starting with three or more
/// backticks or tildes opens a fence, and only a line holding nothing but
/// an equal-or-longer run of the same character closes it.
#[derive(Debug, Default)]
struct Fence(Option<(char, usize)>);

impl Fence {
    /// Takes one line (without its line break); returns whether it's code:
    /// a fence line, or a line inside a fence.
    fn code(&mut self, line: &str) -> bool {
        let run = |t: &str, ch: char| t.len() - t.trim_start_matches(ch).len();
        match self.0 {
            Some((ch, len)) => {
                // The closing run, then only ASCII spaces or tabs.
                let t = line.trim_start_matches([' ', '\t']);
                let n = run(t, ch);
                if n >= len && t[n..].chars().all(|c| c == ' ' || c == '\t') {
                    self.0 = None;
                }
                true
            }
            None => {
                let t = line.trim_start();
                for ch in ['`', '~'] {
                    let n = run(t, ch);
                    if n >= 3 {
                        self.0 = Some((ch, n));
                        return true;
                    }
                }
                false
            }
        }
    }
}

/// A line split into prose and inline code: a code span opens with a run
/// of backticks and closes with a run of the same length on the same line;
/// a run with no partner is prose. Returns `(text, is_code)` pieces, the
/// backtick runs themselves as prose.
fn code_spans(line: &str) -> Vec<(&str, bool)> {
    let b = line.as_bytes();
    let run_at = |i: usize| b[i..].iter().take_while(|c| **c == b'`').count();
    let mut out = Vec::new();
    let (mut i, mut prose_from) = (0, 0);
    while i < b.len() {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let n = run_at(i);
        // The closing run: exactly n backticks.
        let mut j = i + n;
        let mut close = None;
        while j < b.len() {
            if b[j] == b'`' {
                let m = run_at(j);
                if m == n {
                    close = Some(j);
                    break;
                }
                j += m;
            } else {
                j += 1;
            }
        }
        match close {
            Some(j) => {
                out.push((&line[prose_from..i + n], false));
                out.push((&line[i + n..j], true));
                prose_from = j;
                i = j + n;
            }
            None => i += n,
        }
    }
    out.push((&line[prose_from..], false));
    out
}

/// Splits the progress and review-ledger sections out of a body. Each
/// keeps its heading and gets one line in place of its text, made by
/// `summary`. A section runs to the next top-level heading (outside code).
pub fn split_sections(
    body: &str,
    mut summary: impl FnMut(&Section) -> String,
) -> (String, Vec<Section>) {
    let mut out = String::with_capacity(body.len());
    let mut sections = Vec::new();
    let mut current: Option<Section> = None;
    let mut in_code = Fence::default();
    let finish = |s: Section, out: &mut String, summary: &mut dyn FnMut(&Section) -> String| {
        out.push('\n');
        out.push_str(&summary(&s));
        out.push_str("\n\n");
        s
    };
    for line in body.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        let heading = (!in_code.code(bare))
            .then(|| bare.strip_prefix("# "))
            .flatten();
        if let Some(h) = heading {
            if let Some(s) = current.take() {
                sections.push(finish(s, &mut out, &mut summary));
            }
            if let Some(kind) = section_kind(h) {
                out.push_str(line);
                current = Some(Section {
                    kind,
                    heading: h.to_string(),
                    text: String::new(),
                });
                continue;
            }
        }
        match &mut current {
            Some(s) => s.text.push_str(line),
            None => out.push_str(line),
        }
    }
    if let Some(s) = current.take() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&summary(&s));
        out.push('\n');
        sections.push(s);
    }
    (out, sections)
}

/// Rewrites the body lines that hold gate commands: those inside a section
/// whose heading starts "Test Plan" or "Verification" (planned checks).
/// Evidence of what was run stays as it was written.
pub fn gate_sections(body: &str) -> Option<(String, bool)> {
    let mut out = String::with_capacity(body.len());
    let mut in_gates = false;
    let mut in_code = Fence::default();
    let mut dropped = false;
    for line in body.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if !in_code.code(bare)
            && let Some(h) = bare.strip_prefix("# ")
        {
            let h = h.trim().to_ascii_lowercase();
            in_gates = h.starts_with("test plan") || h.starts_with("verification");
        }
        match in_gates.then(|| capability_search(line)).flatten() {
            Some((new, d)) => {
                dropped |= d;
                out.push_str(&new);
            }
            None => out.push_str(line),
        }
    }
    (out != body).then_some((out, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_keys() {
        assert_eq!(
            extension_key("x_acme_billing"),
            Some(Ok(("x-acme.billing".into(), false)))
        );
        assert_eq!(
            extension_key("x_acme_billing_plan"),
            Some(Ok(("x-acme.billing_plan".into(), true)))
        );
        assert_eq!(
            extension_key("x_acme2_b1"),
            Some(Ok(("x-acme2.b1".into(), false)))
        );
        assert_eq!(extension_key("x_owner"), Some(Err(())));
        assert_eq!(extension_key("x_1co_a"), Some(Err(())));
        assert_eq!(extension_key("x_1a_b"), Some(Err(())));
        assert_eq!(extension_key("x_"), Some(Err(())));
        assert_eq!(extension_key("x__b"), Some(Err(())));
        assert_eq!(extension_key("title"), None);
    }

    #[test]
    fn capability_search_gates() {
        assert_eq!(
            capability_search("mdkg capability search \"room sandbox\" --json"),
            Some(("mdkg search \"room sandbox\" --json".into(), false))
        );
        assert_eq!(
            capability_search("- `mdkg capability search \"x\" --kind skill --json` resolves"),
            Some(("- `mdkg search \"x\" --json` resolves".into(), true))
        );
        assert_eq!(
            capability_search("mdkg capability search q --kind=spec --no-cache"),
            Some(("mdkg search q".into(), true))
        );
        assert_eq!(
            capability_search_as(
                "mdkg-next capability search topic --kind skill",
                "mdkg-next"
            ),
            Some(("mdkg-next search topic".into(), true))
        );
        // Prose, and other commands, stay.
        assert_eq!(capability_search("child runtime capability searches"), None);
        assert_eq!(capability_search("a capability search for x"), None);
        assert_eq!(capability_search("mdkg capability list"), None);
        // Another build's name is a command only for that build.
        assert_eq!(capability_search("mdkg-rc capability search x"), None);
        assert!(capability_search_as("mdkg-rc capability search x", "mdkg-rc").is_some());
    }

    fn map_all(t: &Token<'_>) -> Option<String> {
        const KNOWN: &[&str] = &["task-1", "task-2", "task-3", "task-4", "chk-3"];
        KNOWN.contains(&t.id).then(|| format!("<{}>", t.id))
    }

    #[test]
    fn identity_refs_are_whole_words() {
        const G: &str = "6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b";
        const N: &str = "1b2c3d4e-5f60-4718-9a2b-3c4d5e6f7a8b";
        let r = format!("mdkg://{G}/{N}");
        let text = format!(
            "[a]({r}) `{r}` {r}, x{r} {r}x {r}-1 mdkg://{G} mdkg://{G}/{}",
            &N[..35]
        );
        let out = identity_refs(&text, &link_targets(&text), |g, n, written| {
            assert_eq!((g, n, written), (G, N, r.as_str()));
            Some(format!("task-{n}"))
        });
        let t = format!("task-{N}");
        assert_eq!(
            out,
            format!(
                "[a]({t}) `{t}` {t}, x{r} {r}x {r}-1 mdkg://{G} mdkg://{G}/{}",
                &N[..35]
            )
        );
        // Unknown refs stay.
        assert_eq!(
            identity_refs(&text, &link_targets(&text), |_, _, _| None),
            text
        );
        // Inside another URL or a path, or continued by one: as written.
        let text = format!(
            "https://v.example/?target={r} a/{r} ={r} {r}/x {r}?q {r}#h {r}.json \
             [v](https://v.example/?items=a,{r}) x;{r} (a,{r}) <{r}> {r}. \
             [a]({r}),[b]({r}) [v](https://v.example/?items=[x]({r}))"
        );
        let out = identity_refs(&text, &link_targets(&text), |_, n, _| {
            Some(format!("task-{n}"))
        });
        assert_eq!(
            out,
            format!(
                "https://v.example/?target={r} a/{r} ={r} {r}/x {r}?q {r}#h {r}.json \
                 [v](https://v.example/?items=a,{r}) x;{r} (a,{r}) <{t}> {t}. \
                 [a]({t}),[b]({t}) [v](https://v.example/?items=[x]({r}))"
            )
        );
    }

    #[test]
    fn tokens() {
        let (out, unknown) = rewrite_tokens(
            "see task-1, root:task-2 and `mdkg show chk-3`; task-99.",
            map_all,
        );
        assert_eq!(
            out,
            "see <task-1>, <task-2> and `mdkg show <chk-3>`; task-99."
        );
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].text, "task-99");
        // Not inside paths, file names, URLs, words or anchors.
        let text = ".mdkg/work/task-1-x.md task-1-x https://h/task-1 mytask-1 \
                    a.task-1 #task-1 task-1a task-12_b x@task-1";
        let (out, unknown) = rewrite_tokens(text, map_all);
        assert_eq!(out, text);
        let unknown: Vec<&str> = unknown.iter().map(|t| t.text).collect();
        assert_eq!(unknown, ["mytask-1"]);
        // A trailing hyphen or punctuation still ends the token.
        let (out, _) = rewrite_tokens("(task-1) task-2- \"task-3\" task-4's", map_all);
        assert_eq!(out, "(<task-1>) <task-2>- \"<task-3>\" <task-4>'s");
        // Qualifiers are reported.
        let mut seen = Vec::new();
        rewrite_tokens("omni_room_runtime:goal-27 ws2:task-1", |t| {
            seen.push((t.qualifier.map(str::to_string), t.id.to_string()));
            None
        });
        assert_eq!(
            seen,
            vec![
                (Some("omni_room_runtime".into()), "goal-27".into()),
                (Some("ws2".into()), "task-1".into())
            ]
        );
        // Non-ASCII text around tokens is safe.
        let (out, _) = rewrite_tokens("é task-1 ü", map_all);
        assert_eq!(out, "é <task-1> ü");
    }

    #[test]
    fn body_mentions_become_links() {
        let body = "See task-1 and (root:task-2).\n\
                    Run `mdkg show task-1` now.\n\
                    ```\nmdkg done task-3\n```\n\
                    Old link [task-1](../work/task-1-x.md) and [see](task-2).\n\
                    Unknown task-99.\n";
        let (out, unknown) = rewrite_body(
            body,
            |t| {
                map_all(t).map_or(Mention::Unknown, |s| {
                    Mention::Node(s.trim_matches(['<', '>']).replace("task", "T"))
                })
            },
            |_, _| Mention::Unknown,
        );
        assert_eq!(
            out,
            "See [task-1](T-1) and ([root:task-2](T-2)).\n\
             Run `mdkg show T-1` now.\n\
             ```\nmdkg done T-3\n```\n\
             Old link [task-1](../work/task-1-x.md) and [see](T-2).\n\
             Unknown task-99.\n"
        );
        assert_eq!(unknown, ["task-99"]);
        // Double backticks and an unclosed span.
        let (out, _) = rewrite_body(
            "``a task-1`` b task-1 `c task-1\n",
            |t| map_all(t).map_or(Mention::Unknown, |_| Mention::Node("N".into())),
            |_, _| Mention::Unknown,
        );
        assert_eq!(out, "``a N`` b [task-1](N) `c [task-1](N)\n");
        // A node on another branch: the id in code, prose as written.
        let (out, unknown) = rewrite_body(
            "see task-1 and `task-1`\n",
            |_| Mention::CodeOnly("N".into()),
            |_, _| Mention::Unknown,
        );
        assert_eq!(out, "see task-1 and `N`\n");
        assert!(unknown.is_empty());
        // Identity v2 refs: known ones everywhere, a branch-only one in code
        // only, unknown ones reported.
        let g = "6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b";
        let (here, there, none) = (
            "1b2c3d4e-5f60-4718-9a2b-3c4d5e6f7a8b",
            "2c3d4e5f-6071-4829-ab3c-4d5e6f7a8b9c",
            "3d4e5f60-7182-4a3b-8c4d-5e6f708192a3",
        );
        let body = format!(
            "[a](mdkg://{g}/{here}) mdkg://{g}/{there} `mdkg://{g}/{there}` mdkg://{g}/{none}\n\
             ```\nmdkg show mdkg://{g}/{there}\n```\n"
        );
        let (out, unknown) = rewrite_body(
            &body,
            |_| Mention::Unknown,
            |_, n| {
                if n == here {
                    Mention::Node("H".into())
                } else if n == there {
                    Mention::CodeOnly("T".into())
                } else {
                    Mention::Unknown
                }
            },
        );
        assert_eq!(
            out,
            format!("[a](H) mdkg://{g}/{there} `T` mdkg://{g}/{none}\n```\nmdkg show T\n```\n")
        );
        assert_eq!(unknown, [format!("mdkg://{g}/{none}")]);
        // A link whose title holds a code span is still a link, and a
        // legacy mention before it doesn't shift where its target is.
        let body = format!("task-1 then [a](mdkg://{g}/{here} \"use `show`\").\n");
        let (out, _) = rewrite_body(
            &body,
            |t| map_all(t).map_or(Mention::Unknown, |_| Mention::Node("task-0192".into())),
            |_, n| {
                if n == here {
                    Mention::Node("H".into())
                } else {
                    Mention::Unknown
                }
            },
        );
        assert_eq!(out, "[task-1](task-0192) then [a](H \"use `show`\").\n");
    }

    #[test]
    fn code_follows_the_spec_grammar() {
        let node =
            |t: &Token<'_>| map_all(t).map_or(Mention::Unknown, |_| Mention::Node("N".into()));
        // A longer fence holds a shorter one; a tilde fence closes only on
        // tildes; the info string stays.
        let body =
            "````md task-1\n```\ntask-1\n````\ntask-1\n~~~\ntask-1\n```\ntask-1\n~~~~\ntask-1\n";
        let (out, _) = rewrite_body(body, node, |_, _| Mention::Unknown);
        assert_eq!(
            out,
            "````md task-1\n```\nN\n````\n[task-1](N)\n~~~\nN\n```\nN\n~~~~\n[task-1](N)\n"
        );
        // Spans pair equal runs (the ``` `` ``` inside is skipped); a run
        // with no partner is prose.
        let (out, _) = rewrite_body("a ` task-1 ``task-1` x`` `task-1`\n", node, |_, _| {
            Mention::Unknown
        });
        assert_eq!(out, "a ` N ``N` x`` `N`\n");
        let (out, _) = rewrite_body("a `` task-1 ` b\n", node, |_, _| Mention::Unknown);
        assert_eq!(out, "a `` [task-1](N) ` b\n");
        assert_eq!(
            code_spans("a `b` c"),
            vec![("a `", false), ("b", true), ("` c", false)]
        );
    }

    #[test]
    fn sections_move_with_one_line_left() {
        let body = "\n# Overview\n\nKeep.\n\n# Progress (2026-10-19)\n\n- did a\n```\n# not a heading\n```\n\n# Test Plan\n\nKeep too.\n\n# Review ledger\n\n| r | x |\n";
        let (out, sections) = split_sections(body, |s| format!("moved: {}", s.kind.as_str()));
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Progress (2026-10-19)");
        assert_eq!(sections[0].text, "\n- did a\n```\n# not a heading\n```\n\n");
        assert_eq!(sections[1].text, "\n| r | x |\n");
        assert_eq!(
            out,
            "\n# Overview\n\nKeep.\n\n# Progress (2026-10-19)\n\nmoved: progress\n\n# Test Plan\n\nKeep too.\n\n# Review ledger\n\nmoved: review-ledger\n"
        );
        let (same, none) = split_sections("# Overview\n\nx\n", |_| unreachable!());
        assert_eq!(same, "# Overview\n\nx\n");
        assert!(none.is_empty());
    }

    #[test]
    fn gates_only_in_planned_checks() {
        let body = "# Test Plan\n\n- `mdkg capability search \"x\" --json`\n\n# Evidence\n\n- `mdkg capability search \"x\" --json` returned edd-1\n";
        let (out, dropped) = gate_sections(body).unwrap();
        assert!(!dropped);
        assert_eq!(
            out,
            "# Test Plan\n\n- `mdkg search \"x\" --json`\n\n# Evidence\n\n- `mdkg capability search \"x\" --json` returned edd-1\n"
        );
        assert_eq!(
            gate_sections("# Evidence\n\nmdkg capability search x\n"),
            None
        );
        // A path to the binary is the command too (another name is the
        // running build's own: capability_search_as covers it).
        let (out, _) =
            gate_sections("# Test Plan\n\n- `./target/release/mdkg capability search \"x\"`\n")
                .unwrap();
        assert_eq!(
            out,
            "# Test Plan\n\n- `./target/release/mdkg search \"x\"`\n"
        );
    }
}
