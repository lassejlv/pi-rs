//! `YAML.parse(src)` of the `yaml` npm package (2.9.0), which TypeScript pi uses for skill,
//! prompt template and agent frontmatter.
//!
//! A port of the package's lexer, CST parser, composer (core schema, YAML 1.2), `toJS` and
//! pretty error formatting, so values and error messages ("… at line L, column C:" followed by
//! the source excerpt and a caret line) match the package. Warnings are not reported (pi silences
//! `process.emitWarning`).
//!
//! Known gaps: keys whose value is a collection, date or buffer are stringified approximately
//! (the package runs its YAML stringifier); nesting deeper than [`compose::MAX_COMPOSE_DEPTH`]
//! levels reports "Maximum call stack size exceeded" where V8's limit depends on its stack size.

mod compose;
mod lexer;
mod parser;
mod tojs;

use serde_json::Value;

use compose::{Composer, YamlErr};
use parser::Parser;

/// Inputs that may nest deeper than this are parsed on a helper thread with a large stack: the
/// CST parser and the composer recurse once per nesting level (several KiB per level in debug
/// builds), and nesting is bounded only by the input length.
const INLINE_NESTING_BOUND: usize = 64;

/// `YAML.parse(source)`. `Err` holds the message of the thrown error.
pub fn parse(source: &str) -> Result<Value, String> {
    let units: Vec<u16> = source.encode_utf16().collect();
    // Every nesting level needs one of these characters (flow start, block indicator or key).
    let nesting_bound = units
        .iter()
        .filter(|&&c| matches!(c, 0x5B | 0x7B | 0x2D | 0x3F | 0x3A)) // [ { - ? :
        .count();
    if nesting_bound <= INLINE_NESTING_BOUND {
        return parse_units(&units);
    }
    let stack = nesting_bound.saturating_mul(16 << 10).clamp(64 << 20, 512 << 20);
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("yaml-parse".into())
            .stack_size(stack)
            .spawn_scoped(scope, || parse_units(&units))
            .map_err(|e| e.to_string())?
            .join()
            .unwrap_or_else(|_| Err("Maximum call stack size exceeded".to_string()))
    })
}

fn parse_units(units: &[u16]) -> Result<Value, String> {
    let (tokens, line_starts) = Parser::parse(units);
    let composer = Composer::compose(&tokens, units.len() as i64);
    drop_tokens(tokens);
    let mut docs = composer.docs.into_iter();
    let mut doc = docs.next().expect("the composer always yields a document");
    if let Some(second) = docs.next() {
        doc.errors.push(YamlErr {
            pos: (second.range[0], second.range[1]),
            message: "Source contains multiple documents; please use YAML.parseAllDocuments()".into(),
        });
    }
    if let Some(error) = doc.errors.into_iter().next() {
        return Err(prettify_error(units, &line_starts, error));
    }
    let mut ctx = tojs::ToJs::new(&composer.nodes, doc.contents);
    let js = ctx.convert(doc.contents)?;
    Ok(tojs::to_json_value(&js).unwrap_or(Value::Null))
}

/// Drop the CST without recursing once per nesting level.
fn drop_tokens(tokens: Vec<parser::Token>) {
    let mut stack = tokens;
    while let Some(token) = stack.pop() {
        parser::take_children(token, &mut stack);
    }
}

/// `linePos(offset)` of the package's `LineCounter`.
fn line_pos(line_starts: &[i64], offset: i64) -> (i64, i64) {
    let (mut low, mut high) = (0usize, line_starts.len());
    while low < high {
        let mid = (low + high) >> 1;
        if line_starts[mid] < offset {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if line_starts.get(low) == Some(&offset) {
        return (low as i64 + 1, 1);
    }
    if low == 0 {
        return (0, offset);
    }
    let start = line_starts[low - 1];
    (low as i64, offset - start + 1)
}

/// JavaScript `str.substring(start, end)` on UTF-16 code units.
fn substring(s: &[u16], start: i64, end: Option<i64>) -> Vec<u16> {
    let len = s.len() as i64;
    let a = start.clamp(0, len);
    let b = end.unwrap_or(len).clamp(0, len);
    let (a, b) = if a > b { (b, a) } else { (a, b) };
    s[a as usize..b as usize].to_vec()
}

/// `prettifyError(src, lc)(error)`: append the position and a source excerpt.
fn prettify_error(src: &[u16], line_starts: &[i64], error: YamlErr) -> String {
    let mut message = error.message;
    if error.pos.0 == -1 {
        return message;
    }
    let (line, col) = line_pos(line_starts, error.pos.0);
    let end = line_pos(line_starts, error.pos.1);
    message.push_str(&format!(" at line {line}, column {col}"));
    let mut ci = col - 1;
    let ls = |i: i64| -> Option<i64> {
        if i < 0 { None } else { line_starts.get(i as usize).copied() }
    };
    let mut line_str = substring(src, ls(line - 1).unwrap_or(0), ls(line));
    while matches!(line_str.last(), Some(&c) if c == b'\n' as u16 || c == b'\r' as u16) {
        line_str.pop();
    }
    const ELLIPSIS: u16 = 0x2026;
    // Trim to max 80 chars, keeping col position near the middle
    if ci >= 60 && line_str.len() > 80 {
        let trim_start = (ci - 39).min(line_str.len() as i64 - 79);
        let mut s = vec![ELLIPSIS];
        s.extend(substring(&line_str, trim_start, None));
        line_str = s;
        ci -= trim_start - 1;
    }
    if line_str.len() > 80 {
        line_str = substring(&line_str, 0, Some(79));
        line_str.push(ELLIPSIS);
    }
    // Include previous line in context if pointing at line start
    if line > 1 && substring(&line_str, 0, Some(ci)).iter().all(|c| *c == b' ' as u16) {
        // Regexp won't match if start is trimmed
        let mut prev = substring(src, ls(line - 2).unwrap_or(0), ls(line - 1));
        if prev.len() > 80 {
            prev = substring(&prev, 0, Some(79));
            prev.extend("…\n".encode_utf16());
        }
        prev.extend(line_str);
        line_str = prev;
    }
    if line_str.iter().any(|c| *c != b' ' as u16) {
        let mut count = 1;
        if end.0 == line && end.1 > col {
            count = 1.max((end.1 - col).min(80 - ci));
        }
        let pointer = format!("{}{}", " ".repeat(ci.max(0) as usize), "^".repeat(count.max(0) as usize));
        message.push_str(&format!(":\n\n{}\n{pointer}\n", String::from_utf16_lossy(&line_str)));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_core_schema_values() {
        let v = parse("a: 1\nb: 0x1F\nc: 0o17\nd: 1.50\ne: .inf\nf: ~\ng: true\nh: 'x'\ni: [1, two]\n").unwrap();
        assert_eq!(
            v,
            json!({"a": 1, "b": 31, "c": 15, "d": 1.5, "e": null, "f": null, "g": true, "h": "x", "i": [1, "two"]})
        );
        assert_eq!(parse("").unwrap(), Value::Null);
        assert_eq!(parse("# only a comment").unwrap(), Value::Null);
    }

    #[test]
    fn formats_errors_like_the_yaml_package() {
        assert_eq!(
            parse("foo: [bar").unwrap_err(),
            "Flow sequence in block collection must be sufficiently indented and end with a ] at line 1, column 10:\n\nfoo: [bar\n         ^\n"
        );
        assert_eq!(
            parse("description: Use this: when").unwrap_err(),
            "Nested mappings are not allowed in compact mappings at line 1, column 14:\n\ndescription: Use this: when\n             ^\n"
        );
        assert_eq!(
            parse("a: 1\na: 2").unwrap_err(),
            "Map keys must be unique at line 2, column 1:\n\na: 1\na: 2\n^\n"
        );
    }

    #[test]
    fn aliases_and_anchors() {
        assert_eq!(parse("a: &x [1]\nb: *x").unwrap(), json!({"a": [1], "b": [1]}));
        assert_eq!(
            parse("a: *x").unwrap_err(),
            "Unresolved alias (the anchor must be set before the alias): x"
        );
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let deep = format!("a: {}{}", "[".repeat(5000), "]".repeat(5000));
        assert!(parse(&deep).unwrap_err().starts_with("Maximum call stack size exceeded"));
        let block = format!("{}x", "- ".repeat(3000));
        assert!(parse(&block).is_err());
    }
}
