//! Small helpers that reproduce JavaScript string semantics exactly.
//!
//! The reference implementation is TypeScript, so trimming and integer
//! parsing must behave like `String.prototype.trim` and `Number.parseInt`.

/// ECMAScript `WhiteSpace` and `LineTerminator` code points, which is what
/// `String.prototype.trim` removes. This differs from Rust's
/// `char::is_whitespace`: it includes U+FEFF and excludes U+0085.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `Number.parseInt(value, 10)`, returning `None` where JavaScript returns `NaN`.
///
/// Leading whitespace is skipped, an optional sign is accepted, and parsing
/// stops at the first non-digit, so `"12abc"` is 12 and `"3.7"` is 3.
pub fn parse_int(value: &str) -> Option<i64> {
    let s = value.trim_start_matches(is_js_whitespace);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let digits = digits[..end].trim_start_matches('0');
    // Beyond ~1.7976931348623157e308 JavaScript returns Infinity, which then
    // fails `Number.isInteger` ("must be an integer").
    if digits.len() > 309 || (digits.len() == 309 && digits[..17] > *"17976931348623158") {
        return None;
    }
    // Finite but beyond i64: saturate; such values fail every range check.
    let magnitude = if digits.is_empty() {
        0
    } else {
        digits.parse::<i64>().unwrap_or(i64::MAX)
    };
    Some(if negative { -magnitude } else { magnitude })
}

/// Whether `value` equals its JavaScript `toLowerCase()`.
pub fn is_lowercase(value: &str) -> bool {
    value == value.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_matches_javascript() {
        assert_eq!(trim("\u{FEFF}  ---\t"), "---");
        assert_eq!(trim("\u{0085}x"), "\u{0085}x");
    }

    #[test]
    fn parse_int_matches_javascript() {
        assert_eq!(parse_int("3"), Some(3));
        assert_eq!(parse_int("  12abc"), Some(12));
        assert_eq!(parse_int("3.7"), Some(3));
        assert_eq!(parse_int("-4"), Some(-4));
        assert_eq!(parse_int("+5"), Some(5));
        assert_eq!(parse_int("high"), None);
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("-"), None);
        assert_eq!(parse_int("007"), Some(7));
        assert_eq!(parse_int(&"9".repeat(309)), None, "Infinity");
        assert_eq!(
            parse_int(&format!("1{}", "0".repeat(308))),
            Some(i64::MAX),
            "1e308 is finite"
        );
    }
}
