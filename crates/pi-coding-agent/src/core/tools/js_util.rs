//! Small helpers that reproduce JavaScript string and number semantics the tool ports rely on
//! (UTF-16 lengths, `String.prototype.trim`, `Number.prototype.toString`, `localeCompare`).

use std::cmp::Ordering;

/// `str.length` in JavaScript: the number of UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Byte index of the UTF-16 code unit offset `units` (clamped to the string). An offset inside a
/// surrogate pair rounds down to the start of that character.
pub fn utf16_offset_to_byte(s: &str, units: usize) -> usize {
    let mut count = 0usize;
    for (index, ch) in s.char_indices() {
        let next = count + ch.len_utf16();
        if next > units {
            return index;
        }
        count = next;
    }
    s.len()
}

/// `str.slice(0, units)` with UTF-16 offsets.
pub fn utf16_prefix(s: &str, units: usize) -> &str {
    &s[..utf16_offset_to_byte(s, units)]
}

/// Whitespace as `String.prototype.trim` defines it (WhiteSpace + LineTerminator).
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
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `String.prototype.trimEnd`.
pub fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

/// `String(number)` for the values tools print (integers and short decimals).
pub fn js_number_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == value.trunc() && value.abs() < 1e21 {
        return format!("{}", value as i128);
    }
    let text = format!("{value}");
    if text.contains('e') {
        return text;
    }
    text
}

/// `Number.prototype.toFixed(digits)`. Shared by every user-visible money/percent/size format so
/// rounding matches JS (ties round away from zero on the exact binary value, `-0` prints as `0`).
pub fn js_to_fixed(value: f64, digits: usize) -> String {
    // `toFixed` falls back to `String(x)` for NaN, infinities and |x| >= 1e21.
    if !value.is_finite() || value.abs() >= 1e21 {
        return js_number_string(value);
    }
    // `-0` is not `< 0`, so JS prints no sign.
    let value = if value == 0.0 { 0.0 } else { value };
    // JS rounds the exact binary value half-up (away from zero); Rust's formatting rounds the
    // exact value half-to-even. They differ only on exact ties, which we detect on the exact
    // decimal expansion and round away from zero.
    let wide = format!("{:.*}", digits + 40, value.abs());
    let dot = wide.find('.').unwrap_or(wide.len());
    let tail = &wide[dot + 1 + digits..];
    let is_tie = tail.starts_with('5') && tail[1..].bytes().all(|b| b == b'0');
    if !is_tie {
        return format!("{value:.digits$}");
    }
    let factor = 10f64.powi(digits as i32);
    let up = ((value.abs() * factor).trunc() + 1.0) / factor;
    let signed = if value < 0.0 { -up } else { up };
    format!("{signed:.digits$}")
}

/// `String.prototype.localeCompare` without locales: ICU's root collation, as in V8.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    pi_tui::collation::locale_compare(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_helpers() {
        assert_eq!(utf16_len("a😀b"), 4);
        assert_eq!(utf16_prefix("a😀b", 3), "a😀");
        assert_eq!(utf16_prefix("a😀b", 2), "a");
        assert_eq!(js_trim_end("abc \u{feff}\t"), "abc");
    }

    #[test]
    fn numbers() {
        assert_eq!(js_number_string(5.0), "5");
        assert_eq!(js_number_string(2.5), "2.5");
        assert_eq!(js_to_fixed(1.25, 1), "1.3");
        assert_eq!(js_to_fixed(0.5, 1), "0.5");
        assert_eq!(js_to_fixed(1.45, 1), "1.4");
        assert_eq!(js_to_fixed(0.05, 1), "0.1");
        assert_eq!(js_to_fixed(2.5, 0), "3");
        assert_eq!(js_to_fixed(1023.0 / 1024.0, 1), "1.0");
        assert_eq!(js_to_fixed(-0.0, 3), "0.000");
        assert_eq!(js_to_fixed(-0.0001, 3), "-0.000");
        assert_eq!(js_to_fixed(0.125, 2), "0.13");
        assert_eq!(js_to_fixed(-1.25, 1), "-1.3");
        assert_eq!(js_to_fixed(f64::NAN, 2), "NaN");
        assert_eq!(js_to_fixed(f64::INFINITY, 2), "Infinity");
    }

    #[test]
    fn collation() {
        let mut v = vec!["b", "_a", "A", "a", ".hidden", "10", "2", "a-b", "a_b", "é"];
        v.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(v, vec!["_a", ".hidden", "10", "2", "a", "A", "a_b", "a-b", "b", "é"]);
    }
}
