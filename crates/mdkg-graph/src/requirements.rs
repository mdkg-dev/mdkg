//! Requirements in node bodies (a-flow A28). This is body syntax, not
//! frontmatter, and parsing it never rejects the node.

/// A requirement's relationship to a durable design requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Adds,
    Changes,
    Removes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub kind: RelationKind,
    /// The node id as written. Resolution is the validator's job.
    pub node: String,
    pub requirement: Option<String>,
}

/// The EARS pattern recognised in a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarsShape {
    Ubiquitous,
    EventDriven,
    StateDriven,
    Unwanted,
    OptionalFeature,
    Complex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub id: String,
    pub relations: Vec<Relation>,
    pub statement: String,
    pub verify: Option<String>,
    /// One-based line in the body, not in the frontmatter-bearing file.
    pub line: usize,
    pub retired: bool,
    pub ears: Option<EarsShape>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedRequirements {
    pub requirements: Vec<Requirement>,
    /// The one-based body line of each top-level item that did not parse.
    pub bad_lines: Vec<usize>,
    pub section_found: bool,
    /// Items count even when malformed, so an invalid item is not a missing
    /// section as well.
    pub items: usize,
    /// Unsupported syntax or an unclosed fence makes extraction uncertain.
    pub issues: Vec<ParseIssue>,
}

impl ParsedRequirements {
    pub fn uncertain(&self) -> bool {
        !self.issues.is_empty() || !self.bad_lines.is_empty()
    }
}

fn positive_id(s: &str) -> bool {
    let Some(digits) = s.strip_prefix('R') else {
        return false;
    };
    (1..=9).contains(&digits.len())
        && digits
            .bytes()
            .next()
            .is_some_and(|b| (b'1'..=b'9').contains(&b))
        && digits.bytes().all(|b| b.is_ascii_digit())
}

fn relation(s: &str) -> Option<Relation> {
    let (verb, target) = s.split_once(' ')?;
    let kind = match verb {
        "adds" => RelationKind::Adds,
        "changes" => RelationKind::Changes,
        "removes" => RelationKind::Removes,
        _ => return None,
    };
    if target.is_empty() || target.chars().any(char::is_whitespace) {
        return None;
    }
    let (node, requirement) = match target.split_once('#') {
        Some((node, id)) if !node.is_empty() && positive_id(id) => (node, Some(id.to_string())),
        Some(_) => return None,
        None if kind == RelationKind::Adds => (target, None),
        None => return None,
    };
    Some(Relation {
        kind,
        node: node.to_string(),
        requirement,
    })
}

/// Locate the trailing verification suffix without mistaking a nested
/// `(verify:` in its free text for the suffix's opening delimiter.
fn verification_start(text: &str) -> Option<usize> {
    if !text.ends_with(')') {
        return None;
    }
    let delimited = |at: usize| at == 0 || text.as_bytes()[at - 1].is_ascii_whitespace();
    let mut depth = 0usize;
    for (at, character) in text.char_indices().rev() {
        match character {
            ')' => depth += 1,
            '(' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if text[at..].starts_with("(verify:") && delimited(at) {
                        return Some(at);
                    }
                    break;
                }
            }
            _ => {}
        }
    }
    // Free text may contain unbalanced parentheses. In that case the first
    // whitespace-delimited marker is the deterministic fallback.
    text.match_indices("(verify:")
        .find_map(|(at, _)| delimited(at).then_some(at))
}

fn item(text: &str, line: usize) -> Option<Requirement> {
    let text = text.trim();
    let rest = text.strip_prefix('R')?;
    let count = rest.bytes().take_while(u8::is_ascii_digit).count();
    let id = &text[..count + 1];
    if !positive_id(id) {
        return None;
    }
    let mut rest = &text[count + 1..];
    let mut relations = Vec::new();
    let mut retired = false;
    while let Some(after_open) = rest.trim_start().strip_prefix('(') {
        let end = after_open.find(')')?;
        let part = &after_open[..end];
        if part == "retired" {
            if retired || !relations.is_empty() {
                return None;
            }
            retired = true;
        } else {
            if retired {
                return None;
            }
            relations.push(relation(part)?);
        }
        rest = &after_open[end + 1..];
    }
    let text = rest.trim_start().strip_prefix(':')?.trim();
    if text.is_empty() {
        return None;
    }
    let (statement, verify) = match verification_start(text) {
        Some(at) => (
            text[..at].trim(),
            Some(
                text[at + "(verify:".len()..text.len() - 1]
                    .trim()
                    .to_string(),
            ),
        ),
        None => (text, None),
    };
    if statement.is_empty() {
        return None;
    }
    Some(Requirement {
        id: id.to_string(),
        relations,
        statement: statement.to_string(),
        verify,
        line,
        retired,
        ears: (!retired).then(|| ears_shape(statement)).flatten(),
    })
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let count = line.bytes().take_while(|b| *b == b'#').count();
    if count == 0 {
        return None;
    }
    let rest = &line[count..];
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some((count, rest.trim()))
}

/// Return the column and byte offset after leading Markdown indentation.
fn indentation(line: &str, start: usize) -> (usize, usize) {
    let mut column = start;
    let mut bytes = 0;
    for byte in line.bytes() {
        match byte {
            b' ' => column += 1,
            b'\t' => column += 4 - column % 4,
            _ => break,
        }
        bytes += 1;
    }
    (column, bytes)
}

/// A Markdown thematic break is a block separator, not a list item.
fn thematic_break(line: &str) -> bool {
    let mut marker = None;
    let mut count = 0;
    for byte in line.bytes() {
        match byte {
            b' ' | b'\t' => {}
            b'-' | b'*' | b'_' if marker.is_none_or(|m| m == byte) => {
                marker = Some(byte);
                count += 1;
            }
            _ => return false,
        }
    }
    count >= 3
}

/// The item's text, content base column, and actual text column.
fn list_item(line: &str, indent: usize) -> Option<(&str, usize, usize)> {
    let bytes = line.as_bytes();
    let marker = if bytes
        .first()
        .is_some_and(|b| matches!(b, b'-' | b'+' | b'*'))
    {
        1
    } else {
        let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
        if !(1..=9).contains(&digits)
            || !bytes.get(digits).is_some_and(|c| matches!(c, b'.' | b')'))
        {
            return None;
        }
        digits + 1
    };
    let start = indent + marker;
    let (after, spaces) = indentation(&line[marker..], start);
    if marker + spaces == line.len() {
        // An empty list item, including a bare marker, uses one column of
        // padding. Later indented content belongs to that list container.
        return Some(("", start + 1, start + 1));
    }
    if spaces == 0 {
        return None;
    }
    // Markdown uses one column of padding when more than four follow a
    // marker; the remainder starts an indented block within the item.
    let content_indent = if after - start > 4 { start + 1 } else { after };
    Some((&line[marker + spaces..], content_indent, after))
}

struct Pending {
    line: usize,
    text: String,
    has_content: bool,
    content_indent: usize,
    starts_with_invalid_block: bool,
}

struct Fence {
    character: u8,
    length: usize,
    base_indent: usize,
    closing_indent: usize,
    line: usize,
    possible_heading: Option<usize>,
}

fn finish(pending: &mut Option<Pending>, parsed: &mut ParsedRequirements) {
    if let Some(pending) = pending.take() {
        let requirement = if pending.starts_with_invalid_block {
            None
        } else {
            item(&pending.text, pending.line)
        };
        if let Some(requirement) = requirement {
            parsed.requirements.push(requirement);
        } else {
            parsed.bad_lines.push(pending.line);
        }
    }
}

/// A warning about body syntax that can make rendered and parsed structure differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseIssueKind {
    Unsupported,
    UnclosedFence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIssue {
    pub kind: ParseIssueKind,
    /// One-based line in the body.
    pub line: usize,
    pub message: &'static str,
    /// A Requirements-looking heading inside an unclosed fence, if any.
    pub related: Vec<usize>,
}

fn fence_open(text: &str) -> Option<(u8, usize)> {
    let bytes = text.as_bytes();
    let marker = bytes
        .first()
        .copied()
        .filter(|b| matches!(b, b'`' | b'~'))?;
    let length = bytes.iter().take_while(|b| **b == marker).count();
    (length >= 3 && !(marker == b'`' && bytes[length..].contains(&b'`')))
        .then_some((marker, length))
}

// These constructs do not affect structure. Warn so a caller can tell when
// a Markdown renderer might display a different list or heading structure.
fn complete_html_tag(text: &str, mut at: usize, closing: bool) -> bool {
    let bytes = text.as_bytes();
    if closing {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        return bytes.get(at) == Some(&b'>') && bytes[at + 1..].iter().all(u8::is_ascii_whitespace);
    }
    loop {
        let start_space = at;
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if bytes.get(at) == Some(&b'>') {
            return bytes[at + 1..].iter().all(u8::is_ascii_whitespace);
        }
        if bytes.get(at..at + 2) == Some(b"/>") {
            return bytes[at + 2..].iter().all(u8::is_ascii_whitespace);
        }
        if at == start_space
            || !bytes
                .get(at)
                .is_some_and(|b| b.is_ascii_alphabetic() || matches!(b, b'_' | b':'))
        {
            return false;
        }
        at += 1;
        while bytes
            .get(at)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-'))
        {
            at += 1;
        }
        let after_name = at;
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            at = after_name;
            continue;
        }
        at += 1;
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if let Some(quote @ (b'\'' | b'"')) = bytes.get(at).copied() {
            at += 1;
            while bytes.get(at).is_some_and(|b| *b != quote) {
                at += 1;
            }
            if bytes.get(at) != Some(&quote) {
                return false;
            }
            at += 1;
        } else {
            let start = at;
            while bytes.get(at).is_some_and(|b| {
                !b.is_ascii_whitespace() && !matches!(b, b'"' | b'\'' | b'=' | b'<' | b'>' | b'`')
            }) {
                at += 1;
            }
            if at == start {
                return false;
            }
        }
    }
}

fn html_start(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.starts_with(b"<?") {
        return true;
    }
    if bytes.starts_with(b"<![CDATA[") {
        return true;
    }
    if bytes.starts_with(b"<!") && bytes.get(2).is_some_and(u8::is_ascii_alphabetic) {
        return true;
    }
    if bytes.first() != Some(&b'<') {
        return false;
    }
    let closing = bytes.get(1) == Some(&b'/');
    let start = if closing { 2 } else { 1 };
    if !bytes.get(start).is_some_and(u8::is_ascii_alphabetic) {
        return false;
    }
    let mut at = start + 1;
    while bytes
        .get(at)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-')
    {
        at += 1;
    }
    let at_end = at == bytes.len();
    let spaced_or_close = bytes
        .get(at)
        .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>');
    let self_closing = bytes.get(at..at + 2) == Some(b"/>");
    if !at_end && !spaced_or_close && !self_closing {
        return false;
    }
    let tag = &text[start..at];
    if !closing
        && (at_end || spaced_or_close)
        && ["script", "pre", "style", "textarea"]
            .iter()
            .any(|name| tag.eq_ignore_ascii_case(name))
    {
        return true;
    }
    const BLOCK_TAGS: &[&str] = &[
        "address",
        "article",
        "aside",
        "base",
        "basefont",
        "blockquote",
        "body",
        "caption",
        "center",
        "col",
        "colgroup",
        "dd",
        "details",
        "dialog",
        "dir",
        "div",
        "dl",
        "dt",
        "fieldset",
        "figcaption",
        "figure",
        "footer",
        "form",
        "frame",
        "frameset",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "head",
        "header",
        "hr",
        "html",
        "iframe",
        "legend",
        "li",
        "link",
        "main",
        "menu",
        "menuitem",
        "nav",
        "noframes",
        "ol",
        "optgroup",
        "option",
        "p",
        "param",
        "search",
        "section",
        "summary",
        "table",
        "tbody",
        "td",
        "tfoot",
        "th",
        "thead",
        "title",
        "tr",
        "track",
        "ul",
    ];
    BLOCK_TAGS.iter().any(|name| tag.eq_ignore_ascii_case(name))
        || complete_html_tag(text, at, closing)
}

fn table_line(text: &str) -> bool {
    if text.starts_with('|') {
        return true;
    }
    let mut cells = 0;
    let valid = text.split('|').all(|cell| {
        cells += 1;
        let dashes = cell.trim().trim_matches(':');
        dashes.len() >= 3 && dashes.bytes().all(|byte| byte == b'-')
    });
    valid && cells >= 2
}

#[derive(Clone, Copy)]
enum MarkupEnd {
    Blank,
    Closer(&'static str),
    RawText,
}

impl MarkupEnd {
    fn matches(self, text: &str) -> bool {
        match self {
            Self::Blank => text.bytes().all(|byte| matches!(byte, b' ' | b'\t')),
            Self::Closer(closer) => text
                .as_bytes()
                .windows(closer.len())
                .any(|part| part.eq_ignore_ascii_case(closer.as_bytes())),
            Self::RawText => ["</script>", "</pre>", "</style>", "</textarea>"]
                .iter()
                .any(|closer| {
                    text.as_bytes()
                        .windows(closer.len())
                        .any(|part| part.eq_ignore_ascii_case(closer.as_bytes()))
                }),
        }
    }
}

// Used only to decide whether an outside opener gets a warning. The tracker
// never skips lines or decides which headings and items are requirements.
fn markup_end(text: &str) -> MarkupEnd {
    if text.starts_with("<?") {
        return MarkupEnd::Closer("?>");
    }
    if text.starts_with("<![CDATA[") {
        return MarkupEnd::Closer("]]>");
    }
    if text.starts_with("<!") {
        return MarkupEnd::Closer(">");
    }
    let bytes = text.as_bytes();
    if bytes.first() == Some(&b'<') && bytes.get(1) != Some(&b'/') {
        let mut end = 1;
        while bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            end += 1;
        }
        let raw_boundary = bytes
            .get(end)
            .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'>');
        if raw_boundary
            && ["script", "pre", "style", "textarea"]
                .iter()
                .any(|name| text[1..end].eq_ignore_ascii_case(name))
        {
            return MarkupEnd::RawText;
        }
    }
    MarkupEnd::Blank
}

struct OutsideMarkup {
    line: usize,
    end: MarkupEnd,
    base_indent: usize,
}

fn report_unclosed(open: Fence, parsed: &mut ParsedRequirements) {
    parsed.issues.push(ParseIssue {
        kind: ParseIssueKind::UnclosedFence,
        line: open.line,
        message: "unclosed fenced block may hide requirements",
        related: open.possible_heading.into_iter().collect(),
    });
}

/// Parse headings, list markers and fences. All other recognized constructs
/// only warn; their text is read using those same structural rules.
pub fn parse(body: &str) -> ParsedRequirements {
    let mut parsed = ParsedRequirements::default();
    let mut level: Option<usize> = None;
    let mut fence: Option<Fence> = None;
    let mut pending: Option<Pending> = None;
    let mut list_indents: Vec<usize> = Vec::new();
    // Outside the section, only explicit markers establish containers.
    let mut outside_indents: Vec<usize> = Vec::new();
    let mut blank_since_item = false;
    let mut outside_comment: Option<OutsideMarkup> = None;
    let mut outside_html: Option<OutsideMarkup> = None;
    // Outside the section, the bases of containers a plain line closed.
    // CommonMark may read that line as a lazy continuation, keeping them
    // open, so a fence at one of them may hide a heading. Kept for warnings
    // only (orchestrator #10279), until a line below the base really ends
    // the container: a heading, marker or break, or any line after a blank.
    let mut lazy_bases: Vec<usize> = Vec::new();
    let mut lazy_blank = false;
    // Such a fence's line and its container's base, until a line below
    // that base ends the container and the fence with it.
    let mut lazy_fence: Option<(usize, usize)> = None;
    for (index, raw) in body.lines().enumerate() {
        let line_number = index + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let (indent, offset) = indentation(line, 0);
        let trimmed = &line[offset..];
        if let Some(open) = &mut fence {
            if open.base_indent > 0 && indent < open.base_indent && !trimmed.is_empty() {
                let old = fence.take().expect("fence is open");
                report_unclosed(old, &mut parsed);
            } else {
                let bytes = trimmed.as_bytes();
                let run = bytes.iter().take_while(|b| **b == open.character).count();
                if indent >= open.closing_indent
                    && indent - open.closing_indent <= 3
                    && run >= open.length
                    && bytes[run..].iter().all(|b| matches!(b, b' ' | b'\t'))
                {
                    fence = None;
                } else if open.possible_heading.is_none()
                    && indent <= 3
                    && heading(trimmed)
                        .is_some_and(|(depth, title)| depth <= 2 && title == "Requirements")
                {
                    open.possible_heading = Some(line_number);
                }
                continue;
            }
        }
        if trimmed.is_empty() {
            if outside_html
                .as_ref()
                .is_some_and(|open| open.end.matches(line))
            {
                outside_html = None;
            }
            blank_since_item = true;
            lazy_blank = true;
            continue;
        }
        if level.is_none() {
            if lazy_fence.is_some_and(|(_, base)| indent < base) {
                lazy_fence = None;
            }
            let structural = heading(trimmed).is_some()
                || thematic_break(trimmed)
                || list_item(trimmed, indent).is_some()
                || fence_open(trimmed).is_some();
            if structural || lazy_blank {
                lazy_bases.retain(|base| indent >= *base);
            }
            lazy_blank = false;
            while let Some(base) = outside_indents
                .last()
                .copied()
                .filter(|base| indent < *base)
            {
                outside_indents.pop();
                if !structural {
                    lazy_bases.push(base);
                }
            }
        }
        let root_base = pending
            .as_ref()
            .map(|p| p.content_indent)
            .or_else(|| list_indents.first().copied());
        let top_block = indent <= 3 && root_base.is_none_or(|base| indent < base);
        let base = if level.is_none() {
            outside_indents.last().copied().unwrap_or(0)
        } else {
            list_indents.last().copied().unwrap_or(0)
        };
        let mut scan_base = if level.is_some() && top_block {
            0
        } else {
            base
        };
        let mut content = trimmed;
        let mut column = indent;
        let mut marker_tabs = false;
        let mut code_first = false;
        while column >= scan_base && column - scan_base <= 3 {
            let Some((next, next_base, next_column)) = list_item(content, column) else {
                break;
            };
            marker_tabs |= content[..content.len() - next.len()].contains('\t');
            code_first |= next_column.saturating_sub(next_base) >= 4;
            column = next_column;
            scan_base = next_base;
            content = next;
        }
        let indented_code = code_first
            || ((pending.is_none() || blank_since_item) && indent.saturating_sub(base) >= 4);
        let nested_block = pending.is_some()
            && !top_block
            && (heading(trimmed).is_some() || thematic_break(trimmed));
        let column_zero_lazy = pending.is_some()
            && indent == 0
            && !blank_since_item
            && heading(trimmed).is_none()
            && !thematic_break(trimmed)
            && list_item(trimmed, indent).is_none()
            && fence_open(trimmed).is_none();
        let fence_line = !code_first
            && fence_open(content).is_some()
            && column >= scan_base
            && column - scan_base <= 3;
        let section_heading = top_block
            && heading(trimmed).is_some_and(|(depth, title)| depth <= 2 && title == "Requirements");
        let leaves_section = top_block
            && heading(trimmed).is_some_and(|(depth, title)| {
                level.is_some_and(|current| depth <= current) && title != "Requirements"
            });
        let in_section = section_heading || (level.is_some() && !leaves_section);
        if level.is_none() && !fence_line {
            if outside_comment
                .as_ref()
                .is_some_and(|open| indent < open.base_indent)
            {
                outside_comment = None;
            }
            if outside_html
                .as_ref()
                .is_some_and(|open| indent < open.base_indent)
            {
                outside_html = None;
            }
        }
        if !in_section && !fence_line {
            if outside_comment
                .as_ref()
                .is_some_and(|open| indent < open.base_indent || open.end.matches(line))
            {
                outside_comment = None;
            }
            if outside_html
                .as_ref()
                .is_some_and(|open| indent < open.base_indent || open.end.matches(line))
            {
                outside_html = None;
            }
            if !code_first
                && !indented_code
                && outside_comment.is_none()
                && content.starts_with("<!--")
                && !content[4..].contains("-->")
            {
                outside_comment = Some(OutsideMarkup {
                    line: line_number,
                    end: MarkupEnd::Closer("-->"),
                    base_indent: scan_base,
                });
            }
            if outside_html.is_none() && !code_first && !indented_code && html_start(content) {
                let end = markup_end(content);
                if !end.matches(content) {
                    outside_html = Some(OutsideMarkup {
                        line: line_number,
                        end,
                        base_indent: scan_base,
                    });
                }
            }
        }
        if section_heading && level.is_none() {
            if let Some((fence_line, _)) = lazy_fence.take() {
                parsed.issues.push(ParseIssue {
                    kind: ParseIssueKind::Unsupported,
                    line: fence_line,
                    message: "a fence after lazy text outside Requirements may hide its heading",
                    related: vec![line_number],
                });
            }
            for open in [outside_comment.take(), outside_html.take()]
                .into_iter()
                .flatten()
            {
                parsed.issues.push(ParseIssue {
                    kind: ParseIssueKind::Unsupported,
                    line: open.line,
                    message: "HTML outside Requirements may hide its heading",
                    related: vec![line_number],
                });
            }
        }
        if in_section
            && !fence_line
            && (line.contains("<!--")
                || html_start(content)
                || content.starts_with('>')
                || table_line(content)
                || trimmed.starts_with('>')
                || table_line(trimmed)
                || line[..offset].contains('\t')
                || marker_tabs
                || indented_code
                || nested_block
                || column_zero_lazy)
        {
            parsed.issues.push(ParseIssue {
                kind: ParseIssueKind::Unsupported,
                line: line_number,
                message: "unsupported Markdown may change how requirements appear",
                related: Vec::new(),
            });
        }
        if level.is_none() {
            let mut content = trimmed;
            let mut column = indent;
            let mut base = outside_indents.last().copied().unwrap_or(0);
            while column >= base && column - base <= 3 {
                let Some((next, next_base, next_column)) = list_item(content, column) else {
                    break;
                };
                let prefix = &content[..content.len() - next.len()];
                let marker_len = if prefix
                    .as_bytes()
                    .first()
                    .is_some_and(|b| matches!(b, b'-' | b'+' | b'*'))
                {
                    1
                } else {
                    prefix.bytes().take_while(u8::is_ascii_digit).count() + 1
                };
                let padding = &prefix[marker_len..];
                // A marker with only whitespace after it is an empty item,
                // as a bare marker is: it never holds later lines (orch
                // #10279, call 1).
                if next.is_empty()
                    || !(1..=4).contains(&padding.len())
                    || !padding.bytes().all(|byte| byte == b' ')
                {
                    break;
                }
                outside_indents.push(next_base);
                content = next;
                column = next_column;
                base = next_base;
            }
            if lazy_fence.is_none()
                && fence_open(content).is_some()
                && !(column >= base && column - base <= 3)
                && let Some(lazy) = lazy_bases
                    .iter()
                    .copied()
                    .filter(|lazy| column >= *lazy && column - lazy <= 3)
                    .min()
            {
                lazy_fence = Some((line_number, lazy));
            }
            if column >= base
                && column - base <= 3
                && let Some((character, length)) = fence_open(content)
            {
                fence = Some(Fence {
                    character,
                    length,
                    base_indent: base,
                    closing_indent: base,
                    line: line_number,
                    possible_heading: None,
                });
            } else if indent <= 3
                && let Some((depth, title)) = heading(trimmed)
                && depth <= 2
                && title == "Requirements"
            {
                parsed.section_found = true;
                level = Some(depth);
                list_indents.clear();
                outside_indents.clear();
                lazy_bases.clear();
            }
            blank_since_item = false;
            continue;
        }
        if top_block && let Some((depth, title)) = heading(trimmed) {
            finish(&mut pending, &mut parsed);
            list_indents.clear();
            if level.is_some_and(|current| depth <= current) {
                level = None;
            }
            if depth <= 2 && title == "Requirements" {
                parsed.section_found = true;
                level = Some(depth);
            }
            blank_since_item = false;
            continue;
        }
        if top_block && thematic_break(trimmed) {
            finish(&mut pending, &mut parsed);
            list_indents.clear();
            blank_since_item = false;
            continue;
        }
        if top_block && let Some((character, length)) = fence_open(trimmed) {
            finish(&mut pending, &mut parsed);
            list_indents.clear();
            fence = Some(Fence {
                character,
                length,
                base_indent: 0,
                closing_indent: 0,
                line: line_number,
                possible_heading: None,
            });
            blank_since_item = false;
            continue;
        }
        let marker = list_item(trimmed, indent);
        let mut continuation_base = None;
        if let Some(item) = &pending {
            let base = list_indents.last().copied().unwrap_or(item.content_indent);
            let marker_in_parent = marker.is_some() && indent >= item.content_indent;
            let new_top_item = top_block && marker.is_some();
            let at_container = list_indents.binary_search(&indent).is_ok();
            let under_indented = indent < base
                && !at_container
                && !marker_in_parent
                && !new_top_item
                && !blank_since_item;
            if under_indented && indent > 0 && item.has_content {
                continuation_base = Some(base);
            }
        }
        let container_indent = continuation_base.unwrap_or(indent);
        while list_indents
            .last()
            .is_some_and(|base| container_indent < *base)
        {
            list_indents.pop();
        }
        let top_level = indent <= 3 && list_indents.is_empty();
        let within_pending = pending
            .as_ref()
            .is_some_and(|item| indent >= item.content_indent);
        let mut base_indent = list_indents.last().copied().unwrap_or(0);
        let mut content = trimmed;
        let mut content_column = indent;
        let mut saw_list_marker = false;
        while content_column >= base_indent && content_column - base_indent <= 3 {
            let Some((next, next_base, next_column)) = list_item(content, content_column) else {
                break;
            };
            list_indents.push(next_base);
            saw_list_marker = true;
            base_indent = next_base;
            content_column = next_column;
            content = next;
        }
        if let Some((character, length)) = fence_open(content)
            && content_column >= base_indent
            && content_column - base_indent <= 3
        {
            let empty_fenced_example = within_pending
                && !saw_list_marker
                && pending
                    .as_ref()
                    .is_some_and(|item| !item.has_content && !item.starts_with_invalid_block);
            if empty_fenced_example {
                pending = None;
                parsed.items -= 1;
            } else if !within_pending {
                finish(&mut pending, &mut parsed);
            } else if saw_list_marker
                && let Some(item) = &mut pending
                && !item.has_content
            {
                item.starts_with_invalid_block = true;
            }
            fence = Some(Fence {
                character,
                length,
                base_indent,
                closing_indent: base_indent,
                line: line_number,
                possible_heading: None,
            });
            blank_since_item = false;
            continue;
        }
        if top_level && let Some((text, content_indent, text_column)) = marker {
            finish(&mut pending, &mut parsed);
            parsed.items += 1;
            pending = Some(Pending {
                line: line_number,
                text: text.to_string(),
                has_content: !text.bytes().all(|byte| matches!(byte, b' ' | b'\t')),
                content_indent,
                starts_with_invalid_block: text_column - content_indent >= 4,
            });
        } else if within_pending && saw_list_marker && content.is_empty() {
            if let Some(item) = &mut pending
                && !item.has_content
            {
                item.starts_with_invalid_block = true;
            }
        } else if indent > 0 && (within_pending || continuation_base.is_some()) {
            if let Some(item) = &mut pending {
                if !item.has_content && indent.saturating_sub(item.content_indent) >= 4 {
                    item.starts_with_invalid_block = true;
                }
                item.text.push(' ');
                item.text.push_str(trimmed.trim_end_matches([' ', '\t']));
                item.has_content |= !trimmed.bytes().all(|byte| matches!(byte, b' ' | b'\t'));
            }
        } else {
            finish(&mut pending, &mut parsed);
        }
        blank_since_item = false;
    }
    if let Some(open) = fence.take() {
        report_unclosed(open, &mut parsed);
    }
    finish(&mut pending, &mut parsed);
    parsed
}

fn keyword<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let prefix = text.get(..word.len())?;
    let rest = text.get(word.len()..)?;
    (prefix.eq_ignore_ascii_case(word) && rest.starts_with(char::is_whitespace))
        .then(|| rest.trim_start())
}

fn clause(text: &str) -> Option<&str> {
    let (condition, rest) = text.split_once(',')?;
    (!condition.trim().is_empty()).then(|| rest.trim_start())
}

fn condition_clause(text: &str) -> Option<&str> {
    // A later clause keyword after a comma disambiguates comma lists in
    // Where/While conditions. Don't scan into the response after `shall`.
    let first_nonspace = text.find(|c: char| !c.is_whitespace())?;
    let mut scanned = 0;
    for (at, _) in text.match_indices(',') {
        // Each segment is inspected once. Looking for `shall` in every
        // remaining suffix makes repeated While clauses quadratic.
        if text.as_bytes()[scanned..at]
            .windows(b" shall ".len())
            .any(|part| part.eq_ignore_ascii_case(b" shall "))
        {
            break;
        }
        let rest = text[at + 1..].trim_start();
        if first_nonspace < at
            && ["while", "when", "if"]
                .iter()
                .any(|word| keyword(rest, word).is_some())
        {
            return Some(rest);
        }
        scanned = at + 1;
    }
    clause(text)
}

fn if_clause(text: &str) -> Option<&str> {
    text.match_indices(',').find_map(|(at, _)| {
        keyword(text[at + 1..].trim_start(), "then").filter(|_| !text[..at].trim().is_empty())
    })
}

/// Classify the EARS forms of A28, or return `None` for a nonmatching
/// sentence. The component and response must both be nonempty.
pub fn ears_shape(statement: &str) -> Option<EarsShape> {
    let mut rest = statement.trim();
    let mut clauses = Vec::new();
    if let Some(condition) = keyword(rest, "where") {
        rest = condition_clause(condition)?;
        clauses.push(EarsShape::OptionalFeature);
    }
    while let Some(condition) = keyword(rest, "while") {
        rest = condition_clause(condition)?;
        clauses.push(EarsShape::StateDriven);
    }
    if let Some(condition) = keyword(rest, "when") {
        rest = clause(condition)?;
        clauses.push(EarsShape::EventDriven);
    } else if let Some(condition) = keyword(rest, "if") {
        rest = if_clause(condition)?;
        clauses.push(EarsShape::Unwanted);
    }
    if ["where", "while", "when", "if"]
        .iter()
        .any(|word| keyword(rest, word).is_some())
    {
        return None;
    }
    let lower = rest.to_ascii_lowercase();
    let at = lower.find(" shall ")?;
    if rest[..at].trim().is_empty() || rest[at + " shall ".len()..].trim().is_empty() {
        return None;
    }
    Some(match clauses.as_slice() {
        [] => EarsShape::Ubiquitous,
        [one] => *one,
        _ => EarsShape::Complex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_5a_top_level_indentation_does_not_promote_nested_items() {
        for spaces in 0..=3 {
            let body = format!(
                "# Requirements\n{}- R1: The service shall start.\n",
                " ".repeat(spaces)
            );
            let parsed = parse(&body);
            assert_eq!(parsed.requirements.len(), 1, "indentation {spaces}");
            assert_eq!(parsed.requirements[0].id, "R1");
        }
        let parsed = parse(
            "# Requirements\n- R1: The service shall start.\n  - R99: nested detail\n- R2: The service shall stop.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1", "R2"]
        );
        assert!(
            parsed.requirements[0]
                .statement
                .contains("R99: nested detail")
        );
        let parsed = parse("  # Requirements\n   - R1: The service shall start.\n");
        assert!(parsed.section_found);
        assert_eq!(parsed.requirements[0].id, "R1");
        let parsed = parse(
            "# Requirements\n - R1: The service shall start.\n 2. R2: The service shall wait.\n * R3: The service shall stop.\n + R4: The service shall report.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1", "R2", "R3", "R4"]
        );
        let parsed =
            parse("# Requirements\n\t- R99: indented code\n- R1: The service shall start.\n");
        assert_eq!(parsed.requirements.len(), 1);
        assert_eq!(parsed.requirements[0].id, "R1");
    }

    #[test]
    fn review_5a_fences_respect_indentation_and_list_context() {
        for marker in ["```", "~~~"] {
            let indented_code =
                format!("# Requirements\n    {marker}text\n- R1: The service shall start.\n");
            assert_eq!(parse(&indented_code).requirements[0].id, "R1", "{marker}");
            let fake_close = format!(
                "# Requirements\n{marker}\n    {marker}\n- R99: example\n{marker}\n- R1: The service shall start.\n"
            );
            let parsed = parse(&fake_close);
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1"],
                "{marker}"
            );
            for spaces in 0..=3 {
                let padding = " ".repeat(spaces);
                let fenced = format!(
                    "# Requirements\n{padding}{marker}text\n- R99: example\n{padding}{marker}\n- R1: The service shall start.\n"
                );
                let parsed = parse(&fenced);
                assert_eq!(
                    parsed
                        .requirements
                        .iter()
                        .map(|r| r.id.as_str())
                        .collect::<Vec<_>>(),
                    ["R1"],
                    "{marker}, indentation {spaces}"
                );
            }
            let nested = format!(
                "# Requirements\n- R1: The service shall start.\n  {marker}text\n  - R99: example\n  {marker}\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&nested);
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1", "R2"],
                "nested {marker}"
            );
            let deeper_nested = format!(
                "# Requirements\n- R1: The service shall start.\n    {marker}text\n    - R99: example\n    {marker}\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&deeper_nested);
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1", "R2"],
                "nested indentation {marker}"
            );
        }
    }

    #[test]
    fn review_5a_subheading_finishes_the_preceding_item() {
        let parsed = parse(
            "# Requirements\n- R1: The service shall start.\n## Notes\n  (verify: smoke)\n- R2: The service shall stop.\n",
        );
        assert_eq!(parsed.requirements.len(), 2);
        assert_eq!(parsed.requirements[0].statement, "The service shall start.");
        assert_eq!(parsed.requirements[0].verify, None);
        assert_eq!(parsed.requirements[1].id, "R2");
    }

    #[test]
    fn review_5a_verification_can_contain_a_nested_marker() {
        let parsed = parse(
            "# Requirements\n- R1: The service shall start. (verify: grep '(verify: pass)' output.log)\n",
        );
        assert_eq!(parsed.requirements[0].statement, "The service shall start.");
        assert_eq!(
            parsed.requirements[0].verify.as_deref(),
            Some("grep '(verify: pass)' output.log")
        );
        let cases = [
            (
                "The service shall report (verify: label). (verify: smoke)",
                "The service shall report (verify: label).",
                "smoke",
            ),
            (
                "The service shall report. (verify: grep '(' file)",
                "The service shall report.",
                "grep '(' file",
            ),
            (
                "The service shall report (verify: label). (verify: grep '(' file)",
                "The service shall report",
                "label). (verify: grep '(' file",
            ),
        ];
        for (text, statement, verify) in cases {
            let parsed = parse(&format!("# Requirements\n- R1: {text}\n"));
            assert_eq!(parsed.requirements[0].statement, statement, "{text}");
            assert_eq!(
                parsed.requirements[0].verify.as_deref(),
                Some(verify),
                "{text}"
            );
        }
        let parsed = parse("# Requirements\n- R1: The service shall report (details).\n");
        assert_eq!(parsed.requirements[0].verify, None);
    }

    #[test]
    fn review_5a_if_trigger_can_contain_commas() {
        assert_eq!(
            ears_shape(
                "If a request is empty, malformed, or too large, then the service shall reject it."
            ),
            Some(EarsShape::Unwanted)
        );
        assert_eq!(
            ears_shape(
                "Where enabled, while online, IF empty, malformed, or large, THEN the service shall reject it."
            ),
            Some(EarsShape::Complex)
        );
        assert_eq!(ears_shape("If , then the service shall reject it."), None);
        assert_eq!(
            ears_shape("If empty, malformed, the service shall reject it."),
            None
        );
    }

    #[test]
    fn review_5a_complex_clauses_can_have_comma_lists() {
        let cases = [
            (
                "Where input is red, green, or blue, when asked, the service shall respond.",
                EarsShape::Complex,
            ),
            (
                "While input is red, green, or blue, if it fails, then the service shall stop.",
                EarsShape::Complex,
            ),
            (
                "Where input is red, green, or blue, the service shall respond.",
                EarsShape::OptionalFeature,
            ),
            (
                "Where enabled, the service shall report, when needed.",
                EarsShape::OptionalFeature,
            ),
            (
                "While ready, the service shall report, if needed.",
                EarsShape::StateDriven,
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(ears_shape(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn parses_relations_retirement_and_continuations() {
        let body = "# Requirements\n- R1 (changes edd-123#R2) (adds prd-456): When it starts, the agent shall send a notice.\n  Continue here. (verify: test (case))\n- R2 (removes edd-123#R3): The service shall stop.\n- R3 (retired): Old behavior.\n";
        let parsed = parse(body);
        assert_eq!(parsed.items, 3);
        assert!(parsed.bad_lines.is_empty());
        assert_eq!(parsed.requirements[0].line, 2);
        assert_eq!(parsed.requirements[0].relations.len(), 2);
        assert_eq!(
            parsed.requirements[0].relations[0].kind,
            RelationKind::Changes
        );
        assert_eq!(
            parsed.requirements[0].relations[0].requirement.as_deref(),
            Some("R2")
        );
        assert_eq!(parsed.requirements[0].relations[1].kind, RelationKind::Adds);
        assert_eq!(
            parsed.requirements[0].verify.as_deref(),
            Some("test (case)")
        );
        assert!(parsed.requirements[0].statement.ends_with("Continue here."));
        assert_eq!(
            parsed.requirements[1].relations[0].kind,
            RelationKind::Removes
        );
        assert!(parsed.requirements[2].retired);
        assert_eq!(parsed.requirements[2].ears, None);
    }

    #[test]
    fn ignores_fences_and_stops_at_the_sections_boundary() {
        let body = "```md\n# Requirements\n- R1: fake\n```\n## Requirements\n- R2: The worker shall act.\n~~~~\n- R3: fake\n~~~\n- R4: still fake\n~~~~\n  more.\n## Other\n- R5: not a requirement\n# Requirements\n- R6: The worker shall stop.\n";
        let parsed = parse(body);
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R2", "R6"]
        );
        assert_eq!(parsed.requirements[0].statement, "The worker shall act.");
        assert!(parsed.section_found);
        assert!(parsed.bad_lines.is_empty());
    }

    #[test]
    fn malformed_items_are_reported_without_losing_valid_items() {
        let parsed = parse(
            "# Requirements\n- R0: invalid\n- R2 (retired) (adds edd-x): invalid\n- hello\n- R4: The worker shall act.\n",
        );
        assert_eq!(parsed.items, 4);
        assert_eq!(parsed.bad_lines, [2, 3, 4]);
        assert_eq!(parsed.requirements[0].id, "R4");
    }

    #[test]
    fn requirement_ids_match_the_ledger_grammar() {
        let parsed = parse(
            "# Requirements\n- R01: zero prefix\n- R0: zero\n- R1000000000: too long\n- R999999999: The service shall act.\n",
        );
        assert_eq!(parsed.bad_lines, [2, 3, 4]);
        assert_eq!(parsed.requirements[0].id, "R999999999");
    }

    #[test]
    fn prd_requirements_can_be_grouped_under_subheadings() {
        let parsed = parse(
            "# Requirements\n## Functional\n- R1: The service shall respond.\n## Non-functional\n- R2: The service shall respond in 1 second.\n# Other\n- R3: outside the section\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1", "R2"]
        );
        assert!(parsed.bad_lines.is_empty());
    }

    #[test]
    fn bundled_template_examples_are_not_live_requirements() {
        let templates = [
            ("task", include_str!("../bundled/default/task.md")),
            ("bug", include_str!("../bundled/default/bug.md")),
            ("feat", include_str!("../bundled/default/feat.md")),
            ("spike", include_str!("../bundled/default/spike.md")),
            ("edd", include_str!("../bundled/default/edd.md")),
            ("prd", include_str!("../bundled/default/prd.md")),
        ];
        for (kind, text) in templates {
            let parsed = parse(text);
            assert!(parsed.section_found, "{kind}");
            assert_eq!(parsed.items, 0, "{kind}");
            assert!(parsed.requirements.is_empty(), "{kind}");
            assert!(parsed.bad_lines.is_empty(), "{kind}");
        }
    }

    #[test]
    fn recognises_each_ears_form_and_clause_order() {
        let cases = [
            ("The service shall respond.", EarsShape::Ubiquitous),
            (
                "WHEN the user acts, the service SHALL respond.",
                EarsShape::EventDriven,
            ),
            (
                "While online, the service shall respond.",
                EarsShape::StateDriven,
            ),
            (
                "If it fails, then the service shall not retry.",
                EarsShape::Unwanted,
            ),
            (
                "Where enabled, the service shall respond.",
                EarsShape::OptionalFeature,
            ),
            (
                "Where enabled, while online, while ready, when asked, the service shall respond.",
                EarsShape::Complex,
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(ears_shape(text), Some(expected), "{text}");
        }
        for text in [
            "The service should respond.",
            "If it fails, the service shall respond.",
            "When asked, where enabled, the service shall act.",
            "The service shall ",
            " shall act.",
        ] {
            assert_eq!(ears_shape(text), None, "{text}");
        }
    }

    #[test]
    fn review_bb_outdented_fence_finishes_the_previous_item() {
        for marker in ["```", "~~~"] {
            let body = format!(
                "# Requirements\n- R1: The service shall run.\n{marker}\nfirst example\n{marker}\n\n  {marker}md\n- R99: The service shall be fake.\n  {marker}\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.items, 2, "{marker}");
            assert!(parsed.bad_lines.is_empty(), "{marker}");
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1", "R2"],
                "{marker}"
            );
            assert_eq!(
                parsed.requirements[0].statement, "The service shall run.",
                "{marker}"
            );
        }
    }

    #[test]
    fn review_bb_nested_list_fences_do_not_contaminate_requirements() {
        for marker in ["```", "~~~"] {
            let body = format!(
                "# Requirements\n- R1: The service shall run.\n  - {marker}text\n    ## Requirements\n    - R99: example (verify: fake)\n    {marker}\n  (verify: actual)\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.requirements.len(), 2, "{marker}");
            assert_eq!(
                parsed.requirements[0].statement, "The service shall run.",
                "{marker}"
            );
            assert_eq!(
                parsed.requirements[0].verify.as_deref(),
                Some("actual"),
                "{marker}"
            );
            assert_eq!(parsed.requirements[1].id, "R2", "{marker}");

            let body = format!(
                "# Requirements\n- R1: The service shall run.\n  - Details:\n    {marker}md\n    example (verify: fake)\n  (verify: actual)\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.requirements.len(), 2, "{marker}");
            assert_eq!(
                parsed.requirements[0].statement, "The service shall run. - Details:",
                "{marker}"
            );
            assert_eq!(
                parsed.requirements[0].verify.as_deref(),
                Some("actual"),
                "{marker}"
            );
            assert_eq!(parsed.requirements[1].id, "R2", "{marker}");

            let body = format!(
                "# Requirements\n- R1: The service shall run.\n  - Details:\n    - {marker}text\n      example (verify: fake)\n      {marker}\n    - More detail\n  (verify: actual)\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.requirements.len(), 2, "{marker}");
            assert_eq!(
                parsed.requirements[0].statement, "The service shall run. - Details: - More detail",
                "{marker}"
            );
            assert_eq!(
                parsed.requirements[0].verify.as_deref(),
                Some("actual"),
                "{marker}"
            );
            assert_eq!(parsed.requirements[1].id, "R2", "{marker}");
        }
    }

    #[test]
    fn review_bb_fences_on_top_level_list_markers_are_examples() {
        for marker in ["```", "~~~"] {
            let body = format!(
                "# Requirements\n- {marker}md\n  # Requirements\n  - R99: The service shall be fake.\n  {marker}\n- R1: The service shall run.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.items, 1, "{marker}");
            assert!(parsed.bad_lines.is_empty(), "{marker}");
            assert_eq!(parsed.requirements.len(), 1, "{marker}");
            assert_eq!(parsed.requirements[0].id, "R1", "{marker}");
        }
    }

    #[test]
    fn review_bb_fences_remain_scoped_at_several_list_depths() {
        for depth in 0..=5 {
            for marker in ["```", "~~~"] {
                let mut body = String::from("# Requirements\n- R1: The service shall run.\n");
                let mut statement = String::from("The service shall run.");
                for level in 1..=depth {
                    body.push_str(&format!("{}- Detail {level}:\n", " ".repeat(level * 2)));
                    statement.push_str(&format!(" - Detail {level}:"));
                }
                let indent = " ".repeat((depth + 1) * 2);
                let content = " ".repeat((depth + 2) * 2);
                body.push_str(&format!("{indent}- {marker}md\n{content}## Requirements\n{content}- R99: example (verify: fake)\n{content}{marker}\n  (verify: actual)\n- R2: The service shall stop.\n"));
                let parsed = parse(&body);
                assert_eq!(parsed.items, 2, "depth={depth} marker={marker}");
                assert_eq!(
                    parsed.requirements[0].statement, statement,
                    "depth={depth} marker={marker}"
                );
                assert_eq!(parsed.requirements[0].verify.as_deref(), Some("actual"));
                assert_eq!(parsed.requirements[1].id, "R2");
            }
        }
    }

    #[test]
    fn review_bb_nested_marker_columns_and_code_indentation() {
        for marker in ["```", "~~~"] {
            for (item, padding) in [("  1. ", "     "), ("  -\t", "    ")] {
                let body = format!(
                    "# Requirements\n- R1: The service shall run.\n{item}{marker}text\n{padding}example (verify: fake)\n{padding}{marker}\n  (verify: actual)\n- R2: The service shall stop.\n"
                );
                let parsed = parse(&body);
                assert_eq!(parsed.requirements.len(), 2, "{item:?} {marker}");
                assert_eq!(parsed.requirements[0].statement, "The service shall run.");
                assert_eq!(parsed.requirements[0].verify.as_deref(), Some("actual"));
                assert_eq!(parsed.requirements[1].id, "R2");
            }
            let body = format!(
                "# Requirements\n- R1: The service shall run.\n  -     {marker}text\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.requirements.len(), 2, "{marker}");
            assert_eq!(parsed.requirements[1].id, "R2");
        }
    }

    #[test]
    fn review_bb_many_while_clauses_are_supported() {
        let statement = format!(
            "{}the service shall respond.",
            "While ready, ".repeat(100_000)
        );
        assert_eq!(ears_shape(&statement), Some(EarsShape::Complex));
    }

    #[test]
    fn outside_fences_are_tracked_without_list_continuations() {
        for marker in ["```", "~~~"] {
            let closed = format!(
                "# Overview\n- {marker}md\n  # Requirements\n  - R99: example\n  {marker}\n# Requirements\n- R1: The service shall run.\n"
            );
            let parsed = parse(&closed);
            assert!(!parsed.uncertain(), "{marker} {:?}", parsed.issues);
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1"]
            );
            let unclosed = format!(
                "# Overview\n- Example\n  {marker}md\n  example\n# Requirements\n- R1: The service shall run.\n"
            );
            let parsed = parse(&unclosed);
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1"]
            );
            assert!(parsed.issues.iter().any(|i| {
                i.kind == ParseIssueKind::UnclosedFence && i.line == 3 && i.related.is_empty()
            }));
        }
    }

    #[test]
    fn a_nested_item_after_a_fenced_list_example_is_not_top_level() {
        for marker in ["```", "~~~"] {
            let body = format!(
                "# Requirements\n- {marker}md\n  example\n  {marker}\n  - R99: The service shall be fake.\n- R1: The service shall run.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.items, 1, "{marker}");
            assert_eq!(parsed.requirements.len(), 1, "{marker}");
            assert_eq!(parsed.requirements[0].id, "R1", "{marker}");

            let body = format!(
                "# Requirements\n- {marker}md\n  example\n  {marker}\n  # Nested note\n- R1: The service shall run.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.items, 1, "{marker}");
            assert_eq!(parsed.requirements.len(), 1, "{marker}");
            assert_eq!(parsed.requirements[0].id, "R1", "{marker}");
        }
    }

    #[test]
    fn bare_nested_markers_scope_fences_and_preserve_verification() {
        for fence in ["```", "~~~"] {
            for (item, padding) in [
                ("  -", "    "),
                ("  +    ", "    "),
                ("  1.", "     "),
                ("  *\t", "    "),
            ] {
                let body = format!(
                    "# Requirements\n- R1: The service shall run.\n{item}\n{padding}{fence}text\n{padding}example (verify: fake)\n  (verify: smoke)\n- R2: The service shall stop.\n"
                );
                let parsed = parse(&body);
                assert_eq!(parsed.requirements.len(), 2, "{fence} {item:?}");
                assert_eq!(parsed.requirements[0].statement, "The service shall run.");
                assert_eq!(parsed.requirements[0].verify.as_deref(), Some("smoke"));
                assert_eq!(parsed.requirements[1].id, "R2");
            }
        }
        let empty = parse("# Requirements\n-\n- R1: The service shall run.\n");
        assert_eq!(empty.items, 2);
        assert_eq!(empty.bad_lines, [2]);
        let continued = parse("# Requirements\n-\n  R1: The service shall run.\n");
        assert_eq!(continued.requirements.len(), 1);
        assert_eq!(continued.requirements[0].id, "R1");
        assert_eq!(continued.requirements[0].line, 2);

        for body in [
            "# Requirements\n-\n      R99: The service shall be fake.\n- R1: The service shall run.\n",
            "# Requirements\n-\n  -\n    R99: The service shall be fake.\n- R1: The service shall run.\n",
        ] {
            let parsed = parse(body);
            assert_eq!(parsed.bad_lines, [2]);
            assert_eq!(parsed.requirements.len(), 1);
            assert_eq!(parsed.requirements[0].id, "R1");
        }
    }

    #[test]
    fn an_item_starting_with_indented_code_is_not_a_requirement() {
        for list in ["-", "+", "*", "1.", "42)"] {
            let body = format!(
                "# Requirements\n{list}     R99: The service shall be fake.\n- R1: The service shall run.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.items, 2, "{list}");
            assert_eq!(parsed.bad_lines, [2], "{list}");
            assert_eq!(parsed.requirements.len(), 1, "{list}");
            assert_eq!(parsed.requirements[0].id, "R1", "{list}");
            let ordinary = format!("# Requirements\n{list}    R1: The service shall run.\n");
            let parsed = parse(&ordinary);
            assert!(parsed.bad_lines.is_empty(), "{list}");
            assert_eq!(parsed.requirements.len(), 1, "{list}");
            assert_eq!(parsed.requirements[0].id, "R1", "{list}");
        }
    }

    #[test]
    fn a_bare_item_starting_with_a_fenced_example_is_ignored() {
        for marker in ["```", "~~~"] {
            for closing in [format!("  {marker}\n"), String::new()] {
                let body = format!(
                    "# Requirements\n-\n  {marker}md\n  - R99: fake\n{closing}- R1: The service shall run.\n"
                );
                let parsed = parse(&body);
                assert_eq!(parsed.items, 1, "{marker}");
                assert!(parsed.bad_lines.is_empty(), "{marker}");
                assert_eq!(parsed.requirements.len(), 1, "{marker}");
                assert_eq!(parsed.requirements[0].id, "R1", "{marker}");
            }
            let body = format!(
                "# Requirements\n-\n  {marker}md\n  example\n  {marker}\n  - R99: nested\n- R1: The service shall run.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.items, 1, "{marker}");
            assert!(parsed.bad_lines.is_empty(), "{marker}");
            assert_eq!(parsed.requirements[0].id, "R1", "{marker}");
        }
    }

    #[test]
    fn thematic_breaks_are_not_requirement_items() {
        for rule in ["---", "- - -", "***", "* * *", "___", "_ _ _"] {
            let body = format!(
                "# Requirements\n- R1: The service shall start.\n{rule}\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.items, 2, "{rule}");
            assert!(parsed.bad_lines.is_empty(), "{rule}");
            assert_eq!(parsed.requirements.len(), 2, "{rule}");
            assert_eq!(parsed.requirements[0].statement, "The service shall start.");
            assert_eq!(parsed.requirements[1].id, "R2");
        }
        let parsed = parse(
            "# Requirements\n- R1: The service shall start.\n  * * *\n  (verify: smoke)\n- R2: The service shall stop.\n",
        );
        assert_eq!(parsed.requirements.len(), 2);
        assert_eq!(parsed.requirements[0].statement, "The service shall start.");
        assert_eq!(parsed.requirements[0].verify.as_deref(), Some("smoke"));
    }

    #[test]
    fn ordered_list_markers_have_at_most_nine_digits() {
        let parsed = parse(
            "# Requirements\n1234567890. R99: The service shall be fake.\n123456789. R1: The service shall run.\n000000001) R2: The service shall stop.\n",
        );
        assert_eq!(parsed.items, 2);
        assert!(parsed.bad_lines.is_empty());
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1", "R2"]
        );
    }

    #[test]
    fn indented_lazy_continuations_preserve_the_list_context() {
        for nested in ["  ## Detail", "  - R99: nested detail"] {
            let body = format!(
                "## Requirements\n- R1: The service shall start.\n continued.\n{nested}\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.items, 2, "{nested}");
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1", "R2"],
                "{nested}"
            );
            assert!(parsed.requirements[0].statement.contains("continued."));
        }
    }

    #[test]
    fn lazy_continuations_and_empty_items_respect_marker_columns() {
        for padding in 0..=3 {
            for marker in ["-", "1.", "123456789."] {
                for spaces in 1..=4 {
                    let content_column = padding + marker.len() + spaces;
                    for continued_column in 1..content_column {
                        let body = format!(
                            "## Requirements\n{}{marker}{}R1: The service shall start.\n{}continued.\n{}## Detail\n- R2: The service shall stop.\n",
                            " ".repeat(padding),
                            " ".repeat(spaces),
                            " ".repeat(continued_column),
                            " ".repeat(content_column)
                        );
                        let parsed = parse(&body);
                        assert_eq!(
                            parsed
                                .requirements
                                .iter()
                                .map(|r| r.id.as_str())
                                .collect::<Vec<_>>(),
                            ["R1", "R2"],
                            "{padding} {marker} {spaces} {continued_column}"
                        );
                    }
                }
                let content_column = padding + marker.len() + 1;
                for paragraph_column in 1..content_column {
                    let body = format!(
                        "# Requirements\n{}{marker}\n{}R99: fake\n- R1: The service shall run.\n",
                        " ".repeat(padding),
                        " ".repeat(paragraph_column)
                    );
                    let parsed = parse(&body);
                    assert_eq!(
                        parsed.bad_lines,
                        [2],
                        "{padding} {marker} {paragraph_column}"
                    );
                    assert_eq!(parsed.requirements.len(), 1);
                    assert_eq!(parsed.requirements[0].id, "R1");
                }
            }
        }
    }

    #[test]
    fn an_empty_item_cannot_take_an_outdented_paragraph_or_thematic_break() {
        for body in [
            "# Requirements\n-\n R99: The service shall be fake.\n- R1: The service shall run.\n",
            "# Requirements\n-\n  ---\n  R99: The service shall be fake.\n- R1: The service shall run.\n",
        ] {
            let parsed = parse(body);
            assert_eq!(parsed.items, 2);
            assert_eq!(parsed.bad_lines, [2]);
            assert_eq!(parsed.requirements.len(), 1);
            assert_eq!(parsed.requirements[0].id, "R1");
        }
    }

    #[test]
    fn outdented_list_markers_and_fences_still_end_an_item() {
        let parsed = parse(
            "# Requirements\n- R1: The service shall start.\n - R2: The service shall stop.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1", "R2"]
        );
        for fence in ["```", "~~~"] {
            let body = format!(
                "# Requirements\n- R1: The service shall start.\n {fence}md\n- R99: fake\n {fence}\n- R2: The service shall stop.\n"
            );
            let parsed = parse(&body);
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1", "R2"],
                "{fence}"
            );
        }
    }

    #[test]
    fn structure_only_examples_warn_without_quarantining() {
        let cases = [
            (
                "# Requirements\n- R1: The service shall log `a\\` <!-- hidden `\n- R99: The service shall run.\n-->\n- R2: The service shall stop.\n",
                2,
                vec!["R1", "R99", "R2"],
            ),
            (
                "<div>\n\u{a0}\n# Requirements\n- R99: The service shall run.\n",
                1,
                vec!["R99"],
            ),
            (
                "Overview text\n<custom>\n# Requirements\n- R1: The service shall run.\n",
                2,
                vec!["R1"],
            ),
            (
                "# Requirements\n- <script>\n- R1: The service shall run.\n",
                2,
                vec!["R1"],
            ),
            (
                "# Requirements\n> quoted example\n\n- R1: The service shall run.\n",
                2,
                vec!["R1"],
            ),
            (
                "# Requirements\n-     <div>\n- R1: The service shall run.\n",
                2,
                vec!["R1"],
            ),
            (
                "# Requirements\n-     <!--\n- R1: The service shall run.\n",
                2,
                vec!["R1"],
            ),
            (
                "# Requirements\n- R1: The service shall show <span title=\"<!--\">start</span>.\n- R2: The service shall stop.\n",
                2,
                vec!["R1", "R2"],
            ),
            (
                "# Requirements\n- R1: The service shall run.\n> section-level quote\n- R2: The service shall stop.\n",
                3,
                vec!["R1", "R2"],
            ),
            (
                "# Requirements\n- R1: The matcher shall accept\n  | as a literal character.\n",
                3,
                vec!["R1"],
            ),
        ];
        for (body, line, ids) in cases {
            let parsed = parse(body);
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ids,
                "{body:?}: {:?}",
                parsed.issues
            );
            assert!(
                parsed
                    .issues
                    .iter()
                    .any(|issue| issue.kind == ParseIssueKind::Unsupported && issue.line == line),
                "{body:?}: {:?}",
                parsed.issues
            );
            assert!(parsed.uncertain());
        }
        for first in ["-     <div>", "-     <!--"] {
            let body = format!("# Requirements\n{first}\n- R1: The service shall run.\n");
            let parsed = parse(&body);
            assert_eq!(parsed.bad_lines, [2]);
            assert!(
                parsed
                    .issues
                    .iter()
                    .any(|i| i.kind == ParseIssueKind::Unsupported && i.line == 2)
            );
        }
        let parsed =
            parse("# Requirements\n- R1: The matcher shall accept\n  | as a literal character.\n");
        assert!(
            parsed.requirements[0]
                .statement
                .contains("| as a literal character.")
        );
        for block in ["<div>", "<!--", "> quote", "| cell |"] {
            let body = format!("# Requirements\n{block}\n- R99: The service shall run.\n");
            let parsed = parse(&body);
            assert_eq!(parsed.requirements[0].id, "R99");
            assert!(parsed.uncertain());
        }
    }

    #[test]
    fn outside_list_fences_hide_examples_and_end_on_outdent() {
        let body = "# Overview\n- Example:\n    ```md\n  <!-- example\n  # Requirements\n  - R99: The service shall be fake.\n  -->\n    ```\n# Requirements\n- R1: The service shall run.\n";
        let parsed = parse(body);
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1"]
        );
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        for marker in ["-", "7."] {
            for spaces in 1..=4 {
                let base = marker.len() + spaces;
                let body = format!(
                    "# Overview\n{marker}{}Example\n{}```md\n{}# Requirements\n{}- R99: fake\n{}```\n# Requirements\n- R1: The service shall run.\n",
                    " ".repeat(spaces),
                    " ".repeat(base + 2),
                    " ".repeat(base),
                    " ".repeat(base),
                    " ".repeat(base + 2)
                );
                let parsed = parse(&body);
                assert_eq!(
                    parsed
                        .requirements
                        .iter()
                        .map(|r| r.id.as_str())
                        .collect::<Vec<_>>(),
                    ["R1"],
                    "{marker} {spaces}: {:?}",
                    parsed.issues
                );
                assert!(
                    !parsed.uncertain(),
                    "{marker} {spaces}: {:?}",
                    parsed.issues
                );
            }
        }
        let body = "# Overview\n-\n  ```text\n  example\n# Requirements\n- R99: example\n";
        let parsed = parse(body);
        assert!(parsed.requirements.is_empty());
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.kind == ParseIssueKind::UnclosedFence && i.line == 3)
        );
        let body = "# Overview\n- Example\n  ```text\n  example\n# Requirements\n- R1: The service shall run.\n";
        let parsed = parse(body);
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1"]
        );
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.kind == ParseIssueKind::UnclosedFence && i.line == 3)
        );
    }

    #[test]
    fn outside_markup_warns_only_when_it_contains_a_requirements_heading() {
        let parsed = parse(
            "# Overview\n| name | value |\n| --- | --- |\n> quote\n<div>ordinary</div>\n\n# Requirements\n- R1: The service shall run.\n",
        );
        assert_eq!(parsed.requirements[0].id, "R1");
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        let parsed = parse(
            "# Overview\nInline `<!--` and <span title=\"<!--\">text</span>.\n# Requirements\n- R1: The service shall run.\n",
        );
        assert_eq!(parsed.requirements[0].id, "R1");
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        let parsed = parse("# Overview\n| name | value |\n> quote\n");
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        let parsed = parse(
            "# Overview\n<!--\n## Requirements\n- R99: The service shall run.\n-->\n# Requirements\n- R1: The service shall stop.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R99", "R1"]
        );
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.kind == ParseIssueKind::Unsupported && i.line == 2 && i.related == [3])
        );
        let parsed = parse("<div>\n# Requirements\n- R1: The service shall run.\n");
        assert_eq!(parsed.requirements[0].id, "R1");
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.kind == ParseIssueKind::Unsupported && i.line == 1 && i.related == [2])
        );
        let parsed = parse("- <div>\n  example\n# Requirements\n- R1: The service shall run.\n");
        assert_eq!(parsed.requirements[0].id, "R1");
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        for code in ["    <!--", "    <div>", "-     <!--", "-     <div>"] {
            let body =
                format!("# Overview\n{code}\n# Requirements\n- R1: The service shall run.\n");
            let parsed = parse(&body);
            assert_eq!(parsed.requirements[0].id, "R1");
            assert!(!parsed.uncertain(), "{code}: {:?}", parsed.issues);
        }
        for opener in ["  <div>", "  <!--"] {
            let body = format!(
                "- item\n{opener}\n  example\n# Requirements\n- R1: The service shall run.\n"
            );
            let parsed = parse(&body);
            assert_eq!(parsed.requirements[0].id, "R1");
            assert!(!parsed.uncertain(), "{opener}: {:?}", parsed.issues);
        }
        let parsed = parse("<pre/>\n\n# Requirements\n- R1: The service shall run.\n");
        assert_eq!(parsed.requirements[0].id, "R1");
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
    }

    #[test]
    fn a_comment_closer_finishes_its_markdown_line() {
        for body in [
            "<!-- closed --> <!-- trailing\n# Requirements\n- R1: The service shall run.\n",
            "<!-- first\n--> <!-- trailing\n# Requirements\n- R1: The service shall run.\n",
        ] {
            let parsed = parse(body);
            assert_eq!(parsed.requirements[0].id, "R1");
            assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        }
    }

    #[test]
    fn html_openers_warn_without_computing_their_extents() {
        let parsed = parse(
            "# Requirements\n<!doctype html\n- R99: The service shall run.\n>\n- R1: The service shall stop.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R99", "R1"]
        );
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.kind == ParseIssueKind::Unsupported && i.line == 2)
        );
        let parsed = parse("# Requirements\n<div/foo\n- R1: The service shall run.\n");
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        assert_eq!(parsed.requirements[0].id, "R1");
        let parsed = parse(
            "# Requirements\n<pre>\n- R99: The service shall run.\n</STYLE>\n- R1: The service shall stop.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R99", "R1"]
        );
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.kind == ParseIssueKind::Unsupported && i.line == 2)
        );
    }

    #[test]
    fn ordinary_inline_html_is_not_a_block_warning() {
        let parsed = parse(
            "# Requirements\n- R1: The service shall show\n  <b>bold</b> text.\n- R2: The service shall stop.\n",
        );
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        assert_eq!(parsed.requirements.len(), 2);
        assert!(parsed.requirements[0].statement.contains("<b>bold</b>"));
    }

    #[test]
    fn blank_lines_and_fenced_examples_keep_their_boundaries() {
        let parsed = parse(
            "# Requirements\n- R1: The service shall start.\n\n Note: see below.\n  - R2: The service shall stop.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1", "R2"]
        );
        assert!(!parsed.requirements[0].statement.contains("Note:"));
        let parsed = parse(
            "# Requirements\n```html <!-- info -->\n<!-- comment -->\n# Requirements\n- R99: example\n```\n- R1: The service shall run.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1"]
        );
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
        let parsed = parse(
            "# Requirements\n- R1: The service shall run.\n\u{a0}\n  continuation.\n- R2: The service shall stop.\n",
        );
        assert!(parsed.issues.iter().any(|i| i.line == 3));
        assert!(parsed.requirements[0].statement.contains("shall run."));
    }

    #[test]
    fn fence_opener_and_contents_do_not_warn_about_html() {
        let parsed = parse(
            "# Requirements\n- - ```html <!-- info -->\n    <!-- ignored -->\n    ```\n- R1: The service shall run.\n",
        );
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1"]
        );
        assert!(!parsed.uncertain(), "{:?}", parsed.issues);
    }

    #[test]
    fn unclosed_fences_warn_and_identify_a_possible_heading() {
        let parsed = parse("```md\n# Requirements\n- R99: example\n");
        assert!(parsed.requirements.is_empty());
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.kind == ParseIssueKind::UnclosedFence
                    && i.line == 1
                    && i.related == [2])
        );
    }

    #[test]
    fn an_outdented_heading_ends_an_unclosed_list_fence() {
        let parsed = parse("- ```text\n  example\n# Requirements\n- R1: The service shall run.\n");
        assert_eq!(
            parsed
                .requirements
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["R1"]
        );
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.kind == ParseIssueKind::UnclosedFence && i.line == 1)
        );
    }

    #[test]
    fn nonbreaking_space_is_content_not_a_blank_fence_example() {
        for body in [
            "# Requirements\n- \u{a0}\n  ```text\n  example\n  ```\n- R1: The service shall run.\n",
            "# Requirements\n-\n  \u{a0}\n  ```text\n  example\n  ```\n- R1: The service shall run.\n",
        ] {
            let parsed = parse(body);
            assert_eq!(parsed.bad_lines, [2]);
            assert!(parsed.uncertain());
            assert_eq!(
                parsed
                    .requirements
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["R1"]
            );
        }
    }

    #[test]
    fn malformed_items_make_the_node_uncertain() {
        let parsed = parse("# Requirements\n- R0: malformed\n- R1: The service shall run.\n");
        assert_eq!(parsed.bad_lines, [2]);
        assert!(parsed.uncertain());
        assert_eq!(parsed.requirements[0].id, "R1");
    }

    #[test]
    fn many_whitespace_continuations_do_not_rescan_the_item() {
        let body = format!(
            "# Requirements\n- R1: The service shall run.\n{}- R2: The service shall stop.\n",
            "  \u{a0}\n".repeat(8_000)
        );
        let mut samples = Vec::new();
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let parsed = parse(&body);
            samples.push(started.elapsed());
            assert_eq!(parsed.requirements.len(), 2);
            assert_eq!(parsed.requirements[0].id, "R1");
            assert_eq!(parsed.requirements[1].id, "R2");
        }
        assert!(
            samples
                .iter()
                .any(|elapsed| *elapsed < std::time::Duration::from_millis(250)),
            "each 8,000-line parse exceeded 250ms: {samples:?}"
        );
    }

    fn ids(parsed: &ParsedRequirements) -> Vec<&str> {
        parsed.requirements.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn a_marker_with_only_whitespace_never_holds_later_lines() {
        // #10270, finding 1: after `- ` and a blank line the item has ended,
        // so the indented comment is top-level and hides what follows.
        for (marker, block) in [("- ", "<!--"), ("-  ", "<div>"), ("1. ", "<!--")] {
            let body = format!(
                "# Overview\n{marker}\n\n  {block}\n## Requirements\n- R99: The service shall be fake.\n"
            );
            let parsed = parse(&body);
            assert!(parsed.uncertain(), "{body:?}");
            assert!(
                parsed
                    .issues
                    .iter()
                    .any(|i| i.kind == ParseIssueKind::Unsupported && i.line == 4),
                "{body:?}: {:?}",
                parsed.issues
            );
        }
        // The fence variant: the fence stays open, as CommonMark has it.
        let body = "# Overview\n- \n\n  ```md\n# Requirements\n- R99: The service shall be fake.\n";
        let parsed = parse(body);
        assert!(ids(&parsed).is_empty());
        assert!(!parsed.section_found);
        assert!(parsed.issues.iter().any(|i| {
            i.kind == ParseIssueKind::UnclosedFence && i.line == 4 && i.related == [5]
        }));
        // Inside the section an empty item is a bad line, and later items
        // still parse.
        let parsed = parse("# Requirements\n- \n- R1: The service shall run.\n");
        assert_eq!(ids(&parsed), ["R1"]);
    }

    #[test]
    fn a_fence_after_lazy_text_outside_requirements_warns() {
        // #10270, finding 2, with orchestrator #10279's option (a): CommonMark
        // keeps the item open across the lazy `text`, so the fence hides the
        // heading; the parser still extracts, but never silently.
        let body = "# Overview\n- Example\ntext\n     ```md\n  # Requirements\n  - R99: The service shall be fake.\n# Next\n";
        let parsed = parse(body);
        assert!(parsed.uncertain());
        assert!(
            parsed.issues.iter().any(|i| {
                i.kind == ParseIssueKind::Unsupported && i.line == 4 && i.related == [5]
            }),
            "{:?}",
            parsed.issues
        );

        // A line below the item's content ends it and the fence: a heading
        // after that is real, and nothing warns.
        let body = "# Overview\n- Example\ntext\n     ```md\n# Requirements\n- R1: The service shall run.\n";
        let parsed = parse(body);
        assert_eq!(ids(&parsed), ["R1"]);
        assert!(parsed.issues.is_empty(), "{:?}", parsed.issues);

        // A blank line alone doesn't end the item: content indented to it
        // continues it, so the fence still hides the heading.
        let lazy_warning = |body: &str| {
            parse(body)
                .issues
                .iter()
                .any(|i| i.message.contains("lazy text"))
        };
        assert!(lazy_warning(
            "# Overview\n- Example\ntext\n\n     ```md\n  # Requirements\n  - R1: The service shall run.\n"
        ));
        // A heading, a marker, or any line after a blank, below the item's
        // content ends it: the fence is then top-level indented code.
        for between in ["## Other\n", "- Another\n", "\nplain\n"] {
            let body = format!(
                "# Overview\n- Example\ntext\n{between}     ```md\n  # Requirements\n  - R1: The service shall run.\n"
            );
            assert!(!lazy_warning(&body), "{body:?}");
        }
    }

    #[test]
    fn trailing_unicode_whitespace_in_continuations_is_not_kept() {
        let parsed = parse(
            "# Requirements\n- R1: When asked, the service\n  shall answer.\u{a0}\n  (verify: a test)\u{3000}\n- R2: The service shall run.\u{a0}\n",
        );
        let r = &parsed.requirements[0];
        assert_eq!(r.statement, "When asked, the service shall answer.");
        assert_eq!(r.verify.as_deref(), Some("a test"));
        assert_eq!(parsed.requirements[1].statement, "The service shall run.");
        assert!(!parsed.uncertain());
    }
}
