//! Path globs for `affected` rules: repository-relative, `/`-separated.
//!
//! - `**` as a whole segment matches zero or more segments.
//! - `*` matches any run of characters within one segment.
//! - `?` matches one character within one segment.
//! - Everything else matches itself. There are no character classes,
//!   braces or escapes, so a pattern can't mean more than it says.

/// A checked pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    text: String,
    segments: Vec<String>,
}

impl Glob {
    /// Checks a pattern: not empty, relative, no `\`, and no empty, `.` or
    /// `..` segment.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err("a path pattern is empty".into());
        }
        if text.starts_with('/') {
            return Err(format!("`{text}` must be relative to the repository"));
        }
        if text.contains('\\') {
            return Err(format!("`{text}` uses `\\`; patterns use `/`"));
        }
        let segments: Vec<String> = text.split('/').map(String::from).collect();
        if let Some(bad) = segments
            .iter()
            .find(|s| s.is_empty() || *s == "." || *s == "..")
        {
            return Err(format!(
                "`{text}` has an empty, `.` or `..` segment (`{bad}`)"
            ));
        }
        Ok(Self {
            text: text.to_string(),
            segments,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether `path` (repository-relative, `/`-separated) matches.
    pub fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        let pattern: Vec<&str> = self.segments.iter().map(String::as_str).collect();
        match_segments(&pattern, &parts)
    }
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((first, rest)) => match path.split_first() {
            Some((segment, tail)) => match_segment(first, segment) && match_segments(rest, tail),
            None => false,
        },
    }
}

/// `*` and `?` within one segment, by characters.
fn match_segment(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Iterative wildcard matching with backtracking to the last `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some('?') => {
                pi += 1;
                ti += 1;
            }
            Some(c) if *c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        Glob::parse(pattern).unwrap().matches(path)
    }

    #[test]
    fn literal_segments_match_themselves_only() {
        assert!(m("go.mod", "go.mod"));
        assert!(!m("go.mod", "sub/go.mod"));
        assert!(!m("go.mod", "go.modx"));
        assert!(m("a/b.txt", "a/b.txt"));
        assert!(!m("a/b.txt", "a/b/txt"));
    }

    #[test]
    fn star_and_question_stay_within_a_segment() {
        assert!(m("*.md", "README.md"));
        assert!(!m("*.md", "docs/README.md"));
        assert!(m("docs/*.md", "docs/a.md"));
        assert!(!m("docs/*.md", "docs/sub/a.md"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "a/c"));
        assert!(m("*_test.go", "x_test.go"));
        assert!(m("a*b*c", "aXbYc"));
        assert!(!m("a*b*c", "aXbY"));
        assert!(m("*", "anything"));
        assert!(!m("*", "a/b"));
    }

    #[test]
    fn double_star_spans_segments() {
        assert!(m("**/go.mod", "go.mod"));
        assert!(m("**/go.mod", "a/b/go.mod"));
        assert!(m("migrations/**", "migrations/001.sql"));
        assert!(m("migrations/**", "migrations/a/b.sql"));
        assert!(!m("migrations/**", "migrationsx/a.sql"));
        assert!(m("api/**/*.proto", "api/v1/x/y.proto"));
        assert!(m("api/**/*.proto", "api/y.proto"));
        assert!(!m("api/**/*.proto", "api/y.protox"));
        assert!(m("**", "a/b/c"));
    }

    #[test]
    fn a_double_star_inside_a_segment_is_a_single_star() {
        assert!(m("a**b", "aXXb"));
        assert!(!m("a**b", "a/b"));
    }

    #[test]
    fn non_ascii_is_matched_by_character() {
        assert!(m("d?c/*.md", "déc/ü.md"));
    }

    #[test]
    fn patterns_are_anchored_at_both_ends() {
        assert!(
            !m("api", "api/v1/x.proto"),
            "a folder name alone matches only that path"
        );
        assert!(!m("v1/x.proto", "api/v1/x.proto"), "no implicit leading **");
        assert!(!m("api/v1", "api/v1/x.proto"), "no implicit trailing **");
        assert!(!m("x", "xx"));
        assert!(!m("xx", "x"));
        assert!(
            m("api/**", "api"),
            "a trailing ** matches the folder itself"
        );
    }

    #[test]
    fn double_star_matches_zero_or_more_segments_anywhere() {
        assert!(m("a/**/b", "a/b"), "zero segments");
        assert!(m("a/**/b", "a/x/b"));
        assert!(m("a/**/b", "a/x/y/z/b"));
        assert!(!m("a/**/b", "a/x/bb"));
        assert!(m("**/**/x", "x"));
        assert!(m("**/x/**", "x"));
        assert!(m("**/x/**", "p/x/q/r"));
        assert!(!m("**/x/**", "p/xx/q"));
    }

    #[test]
    fn wildcards_match_dotfiles_and_dot_folders() {
        assert!(m("*", ".env.example"), "no hidden-file exception");
        assert!(m("**/*.md", ".github/README.md"));
        assert!(m(".github/**", ".github/workflows/ci.yml"));
        assert!(m("**/.gitkeep", "a/b/.gitkeep"));
    }

    #[test]
    fn a_trailing_slash_is_rejected_not_read_as_a_folder() {
        assert!(Glob::parse("docs/").is_err());
        assert!(Glob::parse("docs/**").is_ok());
    }

    #[test]
    fn non_ascii_segments_match_whole_characters() {
        assert!(m("dócs/**", "dócs/a.md"));
        assert!(!m("docs/**", "dócs/a.md"));
        assert!(m("?.txt", "é.txt"), "? is one character, not one byte");
        assert!(!m("??.txt", "é.txt"));
        assert!(m("*ü*", "aüb"));
        assert!(m("日本/*.go", "日本/x.go"));
    }

    #[test]
    fn rejects_patterns_that_could_mean_more_than_they_say() {
        for bad in ["", "/abs", "a\\b", "a//b", "./a", "a/../b", "a/"] {
            assert!(Glob::parse(bad).is_err(), "{bad:?}");
        }
    }
}
