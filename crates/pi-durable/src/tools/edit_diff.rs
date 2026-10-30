//! Exact and fuzzy replacements retain JavaScript UTF16 offsets and strings.
use crate::{Error, JsonString, Result};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}
pub fn detect_line_ending(text: &JsonString) -> LineEnding {
    match text.units().iter().position(|u| *u == 10) {
        Some(i) if i > 0 && text.units()[i - 1] == 13 => LineEnding::Crlf,
        _ => LineEnding::Lf,
    }
}
pub fn normalize_to_lf(text: &JsonString) -> JsonString {
    let mut out = Vec::new();
    let mut units = text.units().iter().copied().peekable();
    while let Some(unit) = units.next() {
        if unit == 13 {
            if units.peek() == Some(&10) {
                units.next();
            }
            out.push(10);
        } else {
            out.push(unit);
        }
    }
    JsonString::from_utf16(out)
}
pub fn restore_line_endings(text: &JsonString, ending: LineEnding) -> JsonString {
    if ending == LineEnding::Lf {
        return text.clone();
    }
    JsonString::from_utf16(
        text.units()
            .iter()
            .flat_map(|&u| if u == 10 { vec![13, 10] } else { vec![u] })
            .collect(),
    )
}
pub fn strip_bom(text: &JsonString) -> (JsonString, JsonString) {
    if text.units().first() == Some(&0xfeff) {
        (
            JsonString::from("\u{feff}"),
            JsonString::from_utf16(text.units()[1..].to_vec()),
        )
    } else {
        (JsonString::default(), text.clone())
    }
}
fn whitespace(unit: u16) -> bool {
    matches!(unit,0x09..=0x0d|0x20|0xa0|0x1680|0x2000..=0x200a|0x2028|0x2029|0x202f|0x205f|0x3000|0xfeff)
}
pub fn normalize_for_fuzzy_match(text: &JsonString) -> JsonString {
    let mut normalized = Vec::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut Vec<u16>| {
        out.extend(run.nfkc().flat_map(|c| {
            let mut buf = [0; 2];
            c.encode_utf16(&mut buf).to_vec()
        }));
        run.clear();
    };
    for ch in char::decode_utf16(text.units().iter().copied()) {
        match ch {
            Ok(ch) => run.push(ch),
            Err(error) => {
                flush(&mut run, &mut normalized);
                normalized.push(error.unpaired_surrogate());
            }
        }
    }
    flush(&mut run, &mut normalized);
    let mut out = Vec::new();
    for (i, line) in normalized.split(|&u| u == 10).enumerate() {
        if i > 0 {
            out.push(10);
        }
        let end = line
            .iter()
            .rposition(|&u| !whitespace(u))
            .map_or(0, |i| i + 1);
        out.extend_from_slice(&line[..end]);
    }
    for unit in &mut out {
        *unit = match *unit {
            0x2018..=0x201b => 39,
            0x201c..=0x201f => 34,
            0x2010..=0x2015 | 0x2212 => 45,
            0xa0 | 0x2002..=0x200a | 0x202f | 0x205f | 0x3000 => 32,
            other => other,
        };
    }
    JsonString::from_utf16(out)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    pub old_text: JsonString,
    pub new_text: JsonString,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedEditsResult {
    pub base_content: JsonString,
    pub new_content: JsonString,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FuzzyMatchResult {
    pub found: bool,
    pub index: isize,
    pub match_length: usize,
    pub used_fuzzy_match: bool,
    pub content_for_replacement: JsonString,
}
fn find(content: &[u16], needle: &[u16]) -> Option<usize> {
    if needle.is_empty() {
        Some(0)
    } else {
        content
            .windows(needle.len())
            .position(|part| part == needle)
    }
}
pub fn fuzzy_find_text(content: &JsonString, old_text: &JsonString) -> FuzzyMatchResult {
    if let Some(index) = find(content.units(), old_text.units()) {
        return FuzzyMatchResult {
            found: true,
            index: index as isize,
            match_length: old_text.units().len(),
            used_fuzzy_match: false,
            content_for_replacement: content.clone(),
        };
    }
    let base = normalize_for_fuzzy_match(content);
    let needle = normalize_for_fuzzy_match(old_text);
    match find(base.units(), needle.units()) {
        Some(index) => FuzzyMatchResult {
            found: true,
            index: index as isize,
            match_length: needle.units().len(),
            used_fuzzy_match: true,
            content_for_replacement: base,
        },
        None => FuzzyMatchResult {
            found: false,
            index: -1,
            match_length: 0,
            used_fuzzy_match: false,
            content_for_replacement: content.clone(),
        },
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextReplacement {
    pub match_index: usize,
    pub match_length: usize,
    pub new_text: JsonString,
}
fn lines(content: &JsonString) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut start = 0;
    for (index, &unit) in content.units().iter().enumerate() {
        if unit == 10 {
            result.push((start, index + 1));
            start = index + 1;
        }
    }
    if start < content.units().len() {
        result.push((start, content.units().len()));
    }
    result
}
fn apply_replacements(
    content: &[u16],
    replacements: &[TextReplacement],
    offset: usize,
) -> JsonString {
    let mut result = content.to_vec();
    for replacement in replacements.iter().rev() {
        let at = replacement.match_index - offset;
        result.splice(
            at..at + replacement.match_length,
            replacement.new_text.units().iter().copied(),
        );
    }
    JsonString::from_utf16(result)
}
pub fn apply_replacements_preserving_unchanged_lines(
    original: &JsonString,
    base: &JsonString,
    replacements: &[TextReplacement],
) -> Result<JsonString> {
    let original_lines = lines(original);
    let base_lines = lines(base);
    if original_lines.len() != base_lines.len() {
        return Err(Error::message(
            "Cannot preserve unchanged lines because the base content has a different line count.",
        ));
    }
    let mut groups = Vec::<(usize, usize, Vec<TextReplacement>)>::new();
    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|r| r.match_index);
    for replacement in sorted {
        let start = base_lines
            .iter()
            .position(|&(a, b)| replacement.match_index >= a && replacement.match_index < b)
            .ok_or_else(|| Error::message("Replacement range is outside the base content."))?;
        let end = (start..base_lines.len())
            .find(|&i| base_lines[i].1 >= replacement.match_index + replacement.match_length)
            .ok_or_else(|| Error::message("Replacement range is outside the base content."))?
            + 1;
        if let Some(group) = groups.last_mut()
            && start < group.1
        {
            group.1 = group.1.max(end);
            group.2.push(replacement);
        } else {
            groups.push((start, end, vec![replacement]));
        }
    }
    let mut output = JsonString::default();
    let mut previous = 0;
    for (start, end, replacements) in groups {
        for &(a, b) in &original_lines[previous..start] {
            output.append(&JsonString::from_utf16(original.units()[a..b].to_vec()));
        }
        let offset = base_lines[start].0;
        output.append(&apply_replacements(
            &base.units()[offset..base_lines[end - 1].1],
            &replacements,
            offset,
        ));
        previous = end;
    }
    for &(a, b) in &original_lines[previous..] {
        output.append(&JsonString::from_utf16(original.units()[a..b].to_vec()));
    }
    Ok(output)
}
fn occurrences(content: &JsonString, needle: &JsonString) -> isize {
    let content = normalize_for_fuzzy_match(content);
    let needle = normalize_for_fuzzy_match(needle);
    if needle.units().is_empty() {
        return content.units().len() as isize - 1;
    }
    let mut count = 0;
    let mut rest = content.units();
    while let Some(i) = find(rest, needle.units()) {
        count += 1;
        rest = &rest[i + needle.units().len()..];
    }
    count
}
fn message(parts: impl IntoIterator<Item = JsonString>) -> Error {
    let mut text = JsonString::default();
    for part in parts {
        text.append(&part);
    }
    Error::host("Error", text)
}
pub fn apply_edits_to_normalized_content(
    content: &JsonString,
    edits: &[Edit],
    path: &JsonString,
) -> Result<AppliedEditsResult> {
    let edits = edits
        .iter()
        .map(|e| Edit {
            old_text: normalize_to_lf(&e.old_text),
            new_text: normalize_to_lf(&e.new_text),
        })
        .collect::<Vec<_>>();
    let single = edits.len() == 1;
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.units().is_empty() {
            return Err(message([
                if single {
                    "oldText must not be empty in ".into()
                } else {
                    format!("edits[{i}].oldText must not be empty in ").into()
                },
                path.clone(),
                ".".into(),
            ]));
        }
    }
    let fuzzy = edits
        .iter()
        .any(|e| fuzzy_find_text(content, &e.old_text).used_fuzzy_match);
    let base = if fuzzy {
        normalize_for_fuzzy_match(content)
    } else {
        content.clone()
    };
    let mut matches = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let found = fuzzy_find_text(&base, &edit.old_text);
        if !found.found {
            return Err(message([
                if single {
                    "Could not find the exact text in ".into()
                } else {
                    format!("Could not find edits[{i}] in ").into()
                },
                path.clone(),
                if single {
                    ". The old text must match exactly including all whitespace and newlines."
                        .into()
                } else {
                    ". The oldText must match exactly including all whitespace and newlines.".into()
                },
            ]));
        }
        let count = occurrences(&base, &edit.old_text);
        if count > 1 {
            return Err(message([
                if single {
                    format!("Found {count} occurrences of the text in ").into()
                } else {
                    format!("Found {count} occurrences of edits[{i}] in ").into()
                },
                path.clone(),
                if single {
                    ". The text must be unique. Please provide more context to make it unique."
                        .into()
                } else {
                    ". Each oldText must be unique. Please provide more context to make it unique."
                        .into()
                },
            ]));
        }
        matches.push((
            i,
            TextReplacement {
                match_index: found.index as usize,
                match_length: found.match_length,
                new_text: edit.new_text.clone(),
            },
        ));
    }
    matches.sort_by_key(|(_, r)| r.match_index);
    for pair in matches.windows(2) {
        if pair[0].1.match_index + pair[0].1.match_length > pair[1].1.match_index {
            return Err(message([
                format!("edits[{}] and edits[{}] overlap in ", pair[0].0, pair[1].0).into(),
                path.clone(),
                ". Merge them into one edit or target disjoint regions.".into(),
            ]));
        }
    }
    let replacements = matches.into_iter().map(|(_, r)| r).collect::<Vec<_>>();
    let new_content = if fuzzy {
        apply_replacements_preserving_unchanged_lines(content, &base, &replacements)?
    } else {
        apply_replacements(base.units(), &replacements, 0)
    };
    if new_content == *content {
        return Err(message([
            "No changes made to ".into(),
            path.clone(),
            if single {
                ". The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.".into()
            } else {
                ". The replacements produced identical content.".into()
            },
        ]));
    }
    Ok(AppliedEditsResult {
        base_content: content.clone(),
        new_content,
    })
}
// The shared diff engine compares complete line tokens. Encoding UTF16 units as
// Unicode scalars is lossless; preserving CR/LF keeps its tokenizer unchanged.
fn encoded(text: &JsonString) -> String {
    text.units()
        .iter()
        .map(|&u| {
            if u == 10 || u == 13 {
                char::from_u32(u as u32).unwrap()
            } else {
                char::from_u32(0x10000 + u as u32).unwrap()
            }
        })
        .collect()
}
fn decoded(text: &str) -> JsonString {
    JsonString::from_utf16(
        text.chars()
            .flat_map(|ch| {
                let n = ch as u32;
                if (0x10000..=0x1ffff).contains(&n) {
                    vec![(n - 0x10000) as u16]
                } else {
                    let mut buf = [0; 2];
                    ch.encode_utf16(&mut buf).to_vec()
                }
            })
            .collect(),
    )
}
pub fn generate_unified_patch(
    path: &JsonString,
    old: &JsonString,
    new: &JsonString,
    context: usize,
) -> JsonString {
    decoded(&pi_json::diff::create_two_files_patch(
        &encoded(path),
        &encoded(path),
        &encoded(old),
        &encoded(new),
        None,
        None,
        context,
        pi_json::diff::FILE_HEADERS_ONLY,
    ))
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffString {
    pub diff: JsonString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
}
pub fn generate_diff_string(old: &JsonString, new: &JsonString, context: usize) -> DiffString {
    let parts = pi_json::diff::diff_lines(&encoded(old), &encoded(new));
    let width = (old.units().iter().filter(|&&u| u == 10).count() + 1)
        .max(new.units().iter().filter(|&&u| u == 10).count() + 1)
        .to_string()
        .len();
    let (mut old_line, mut new_line) = (1, 1);
    let mut changed = false;
    let mut first = None;
    let mut output = Vec::<JsonString>::new();
    for (i, part) in parts.iter().enumerate() {
        let value = decoded(&part.value);
        let mut raw = value.units().split(|&u| u == 10).collect::<Vec<_>>();
        if raw.last() == Some(&&[][..]) {
            raw.pop();
        }
        if part.added || part.removed {
            first.get_or_insert(new_line);
            for line in raw {
                let mut text = JsonString::from(format!(
                    "{}{:>width$} ",
                    if part.added { '+' } else { '-' },
                    if part.added { new_line } else { old_line }
                ));
                text.append(&JsonString::from_utf16(line.to_vec()));
                output.push(text);
                if part.added {
                    new_line += 1;
                } else {
                    old_line += 1;
                }
            }
            changed = true;
        } else {
            let trailing = parts.get(i + 1).is_some_and(|p| p.added || p.removed);
            let (head, tail) = match (changed, trailing) {
                (true, true) if raw.len() <= context.saturating_mul(2) => (raw.len(), 0),
                (true, true) => (context.min(raw.len()), context.min(raw.len())),
                (true, false) => (context.min(raw.len()), 0),
                (false, true) => (0, context.min(raw.len())),
                _ => {
                    old_line += raw.len();
                    new_line += raw.len();
                    changed = false;
                    continue;
                }
            };
            let skipped = raw.len() - head - tail;
            for (index, line) in raw.iter().enumerate() {
                if index == head && skipped > 0 {
                    output.push(format!(" {:width$} ...", "").into());
                    old_line += skipped;
                    new_line += skipped;
                }
                if index >= head && index < raw.len() - tail {
                    continue;
                }
                let mut text = JsonString::from(format!(" {:>width$} ", old_line));
                text.append(&JsonString::from_utf16(line.to_vec()));
                output.push(text);
                old_line += 1;
                new_line += 1;
            }
            changed = false;
        }
    }
    let mut diff = JsonString::default();
    for (i, line) in output.into_iter().enumerate() {
        if i > 0 {
            diff.append(&"\n".into());
        }
        diff.append(&line);
    }
    DiffString {
        diff,
        first_changed_line: first,
    }
}
