//! Port of the parts of the `diff` npm package (jsdiff 8.0.4) that pi uses: the Myers diff engine
//! (`Diff.prototype.diff`), `diffLines`, `structuredPatch`, `formatPatch` and
//! `createTwoFilesPatch`. Output matches jsdiff exactly (same tie-breaking between equally short
//! edit scripts), which keeps the edit tool's diffs and patches identical to the TS version.

/// One change object (`{ value, added, removed, count }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub value: String,
    pub added: bool,
    pub removed: bool,
    pub count: usize,
}

#[derive(Clone, Copy)]
struct Path {
    old_pos: i64,
    last: Option<usize>,
}

struct Component {
    count: usize,
    added: bool,
    removed: bool,
    prev: Option<usize>,
}

struct Engine<'a, T> {
    old: &'a [T],
    new: &'a [T],
    equals: &'a dyn Fn(&T, &T) -> bool,
    arena: Vec<Component>,
}

impl<T> Engine<'_, T> {
    fn add_to_path(&mut self, path: Path, added: bool, removed: bool, old_pos_inc: i64) -> Path {
        let last = path.last;
        let component = match last {
            Some(index)
                if self.arena[index].added == added && self.arena[index].removed == removed =>
            {
                Component {
                    count: self.arena[index].count + 1,
                    added,
                    removed,
                    prev: self.arena[index].prev,
                }
            }
            _ => Component {
                count: 1,
                added,
                removed,
                prev: last,
            },
        };
        self.arena.push(component);
        Path {
            old_pos: path.old_pos + old_pos_inc,
            last: Some(self.arena.len() - 1),
        }
    }

    fn extract_common(&mut self, base: &mut Path, diagonal: i64) -> i64 {
        let new_len = self.new.len() as i64;
        let old_len = self.old.len() as i64;
        let mut old_pos = base.old_pos;
        let mut new_pos = old_pos - diagonal;
        let mut common = 0usize;
        while new_pos + 1 < new_len
            && old_pos + 1 < old_len
            && (self.equals)(
                &self.old[(old_pos + 1) as usize],
                &self.new[(new_pos + 1) as usize],
            )
        {
            new_pos += 1;
            old_pos += 1;
            common += 1;
        }
        if common > 0 {
            self.arena.push(Component {
                count: common,
                added: false,
                removed: false,
                prev: base.last,
            });
            base.last = Some(self.arena.len() - 1);
        }
        base.old_pos = old_pos;
        new_pos
    }

    /// Components in order: `(count, added, removed)`.
    fn components(&self, last: Option<usize>) -> Vec<(usize, bool, bool)> {
        let mut out = Vec::new();
        let mut current = last;
        while let Some(index) = current {
            let c = &self.arena[index];
            out.push((c.count, c.added, c.removed));
            current = c.prev;
        }
        out.reverse();
        out
    }

    fn run(&mut self) -> Vec<(usize, bool, bool)> {
        let new_len = self.new.len() as i64;
        let old_len = self.old.len() as i64;
        let max_edit_length = new_len + old_len;
        let offset = max_edit_length + 1;
        let idx = |diagonal: i64| (diagonal + offset) as usize;
        let mut best: Vec<Option<Path>> = vec![None; (2 * offset + 1) as usize];

        let mut first = Path {
            old_pos: -1,
            last: None,
        };
        let new_pos = self.extract_common(&mut first, 0);
        best[idx(0)] = Some(first);
        if first.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
            return self.components(first.last);
        }

        let mut min_diagonal = i64::MIN;
        let mut max_diagonal = i64::MAX;
        let mut edit_length = 1i64;
        while edit_length <= max_edit_length {
            let mut diagonal = min_diagonal.max(-edit_length);
            while diagonal <= max_diagonal.min(edit_length) {
                let remove_path = best[idx(diagonal - 1)].take();
                let add_path = best[idx(diagonal + 1)];
                let can_add = add_path.is_some_and(|p| {
                    let add_new_pos = p.old_pos - diagonal;
                    0 <= add_new_pos && add_new_pos < new_len
                });
                let can_remove = remove_path.is_some_and(|p| p.old_pos + 1 < old_len);
                if !can_add && !can_remove {
                    best[idx(diagonal)] = None;
                    diagonal += 2;
                    continue;
                }
                let mut base = if !can_remove
                    || (can_add && remove_path.map(|p| p.old_pos) < add_path.map(|p| p.old_pos))
                {
                    self.add_to_path(add_path.expect("add path"), true, false, 0)
                } else {
                    self.add_to_path(remove_path.expect("remove path"), false, true, 1)
                };
                let new_pos = self.extract_common(&mut base, diagonal);
                if base.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                    return self.components(base.last);
                }
                best[idx(diagonal)] = Some(base);
                if base.old_pos + 1 >= old_len {
                    max_diagonal = max_diagonal.min(diagonal - 1);
                }
                if new_pos + 1 >= new_len {
                    min_diagonal = min_diagonal.max(diagonal + 1);
                }
                diagonal += 2;
            }
            edit_length += 1;
        }
        Vec::new()
    }
}

/// Run jsdiff's Myers engine over tokens. Returns `(count, added, removed)` components with token
/// counts; [`diff_strings`] joins the token values.
pub fn diff_token_components<T>(
    old: &[T],
    new: &[T],
    equals: &dyn Fn(&T, &T) -> bool,
) -> Vec<(usize, bool, bool)> {
    let mut engine = Engine {
        old,
        new,
        equals,
        arena: Vec::new(),
    };
    engine.run()
}

/// `Diff.prototype.diff` over string tokens (after `removeEmpty`), joining values with `""`.
/// `use_longest_token` mirrors the subclasses that keep the longer of two equal tokens.
pub fn diff_strings(
    old_tokens: &[String],
    new_tokens: &[String],
    equals: &dyn Fn(&String, &String) -> bool,
    use_longest_token: bool,
) -> Vec<Change> {
    let old: Vec<String> = old_tokens
        .iter()
        .filter(|t| !t.is_empty())
        .cloned()
        .collect();
    let new: Vec<String> = new_tokens
        .iter()
        .filter(|t| !t.is_empty())
        .cloned()
        .collect();
    let components = diff_token_components(&old, &new, equals);
    let mut changes = Vec::with_capacity(components.len());
    let (mut new_pos, mut old_pos) = (0usize, 0usize);
    for (count, added, removed) in components {
        let value = if !removed {
            let value = if !added && use_longest_token {
                new[new_pos..new_pos + count]
                    .iter()
                    .enumerate()
                    .map(|(i, value)| {
                        let old_value = &old[old_pos + i];
                        if old_value.len() > value.len() {
                            old_value.clone()
                        } else {
                            value.clone()
                        }
                    })
                    .collect::<String>()
            } else {
                new[new_pos..new_pos + count].concat()
            };
            new_pos += count;
            if !added {
                old_pos += count;
            }
            value
        } else {
            let value = old[old_pos..old_pos + count].concat();
            old_pos += count;
            value
        };
        changes.push(Change {
            value,
            added,
            removed,
            count,
        });
    }
    changes
}

/// jsdiff's line tokenizer (`newlineIsToken: false`, no `stripTrailingCr`): each token is a line
/// with its `\n` or `\r\n` terminator.
pub fn tokenize_lines(value: &str) -> Vec<String> {
    // value.split(/(\n|\r\n)/) keeps separators at odd indices.
    let mut parts: Vec<&str> = Vec::new();
    let bytes = value.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            parts.push(&value[start..i]);
            parts.push(&value[i..i + 1]);
            i += 1;
            start = i;
        } else if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            parts.push(&value[start..i]);
            parts.push(&value[i..i + 2]);
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(&value[start..]);
    if parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    let mut lines: Vec<String> = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index % 2 == 1 {
            if let Some(last) = lines.last_mut() {
                last.push_str(part);
            }
        } else {
            lines.push(part.to_string());
        }
    }
    lines
}

/// `Diff.diffLines(oldStr, newStr)` with default options.
pub fn diff_lines(old_str: &str, new_str: &str) -> Vec<Change> {
    diff_strings(
        &tokenize_lines(old_str),
        &tokenize_lines(new_str),
        &|a, b| a == b,
        false,
    )
}

/// One hunk of a structured patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
    pub lines: Vec<String>,
}

/// Result of [`structured_patch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredPatch {
    pub old_file_name: String,
    pub new_file_name: String,
    pub old_header: Option<String>,
    pub new_header: Option<String>,
    pub hunks: Vec<Hunk>,
}

/// Split text into lines that keep their trailing `\n` (jsdiff's patch `splitLines`).
fn split_lines_keep_newline(text: &str) -> Vec<String> {
    let has_trailing_nl = text.ends_with('\n');
    let mut result: Vec<String> = text.split('\n').map(|line| format!("{line}\n")).collect();
    if has_trailing_nl {
        result.pop();
    } else if let Some(last) = result.last_mut() {
        last.pop();
    }
    result
}

/// `structuredPatch(oldFileName, newFileName, oldStr, newStr, oldHeader, newHeader, { context })`.
pub fn structured_patch(
    old_file_name: &str,
    new_file_name: &str,
    old_str: &str,
    new_str: &str,
    old_header: Option<&str>,
    new_header: Option<&str>,
    context: usize,
) -> StructuredPatch {
    struct Entry {
        added: bool,
        removed: bool,
        lines: Vec<String>,
    }
    let mut diff: Vec<Entry> = diff_lines(old_str, new_str)
        .into_iter()
        .map(|change| Entry {
            added: change.added,
            removed: change.removed,
            lines: split_lines_keep_newline(&change.value),
        })
        .collect();
    // Append an empty value to make cleanup easier.
    diff.push(Entry {
        added: false,
        removed: false,
        lines: Vec::new(),
    });

    let context_lines =
        |lines: &[String]| -> Vec<String> { lines.iter().map(|l| format!(" {l}")).collect() };

    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut old_range_start, mut new_range_start) = (0i64, 0i64);
    let mut cur_range: Vec<String> = Vec::new();
    let (mut old_line, mut new_line) = (1i64, 1i64);
    for i in 0..diff.len() {
        let current = &diff[i];
        let lines = &current.lines;
        if current.added || current.removed {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if i > 0 {
                    let prev = &diff[i - 1];
                    cur_range = if context > 0 {
                        let start = prev.lines.len().saturating_sub(context);
                        context_lines(&prev.lines[start..])
                    } else {
                        Vec::new()
                    };
                    old_range_start -= cur_range.len() as i64;
                    new_range_start -= cur_range.len() as i64;
                }
            }
            let prefix = if current.added { '+' } else { '-' };
            for line in lines {
                cur_range.push(format!("{prefix}{line}"));
            }
            if current.added {
                new_line += lines.len() as i64;
            } else {
                old_line += lines.len() as i64;
            }
        } else {
            if old_range_start != 0 {
                if lines.len() <= context * 2 && i + 2 < diff.len() {
                    // Overlapping
                    cur_range.extend(context_lines(lines));
                } else {
                    let context_size = lines.len().min(context);
                    cur_range.extend(context_lines(&lines[..context_size]));
                    hunks.push(Hunk {
                        old_start: old_range_start,
                        old_lines: old_line - old_range_start + context_size as i64,
                        new_start: new_range_start,
                        new_lines: new_line - new_range_start + context_size as i64,
                        lines: std::mem::take(&mut cur_range),
                    });
                    old_range_start = 0;
                    new_range_start = 0;
                }
            }
            old_line += lines.len() as i64;
            new_line += lines.len() as i64;
        }
    }

    // Eliminate the trailing `\n` from each line and add "\ No newline at end of file" where needed.
    for hunk in &mut hunks {
        let mut out = Vec::with_capacity(hunk.lines.len());
        for line in hunk.lines.drain(..) {
            if let Some(stripped) = line.strip_suffix('\n') {
                out.push(stripped.to_string());
            } else {
                out.push(line);
                out.push("\\ No newline at end of file".to_string());
            }
        }
        hunk.lines = out;
    }

    StructuredPatch {
        old_file_name: old_file_name.to_string(),
        new_file_name: new_file_name.to_string(),
        old_header: old_header.map(str::to_string),
        new_header: new_header.map(str::to_string),
        hunks,
    }
}

/// Header options of `formatPatch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderOptions {
    pub include_index: bool,
    pub include_underline: bool,
    pub include_file_headers: bool,
}

/// `Diff.INCLUDE_HEADERS`.
pub const INCLUDE_HEADERS: HeaderOptions = HeaderOptions {
    include_index: true,
    include_underline: true,
    include_file_headers: true,
};

/// `Diff.FILE_HEADERS_ONLY`.
pub const FILE_HEADERS_ONLY: HeaderOptions = HeaderOptions {
    include_index: false,
    include_underline: false,
    include_file_headers: true,
};

/// `Diff.OMIT_HEADERS`.
pub const OMIT_HEADERS: HeaderOptions = HeaderOptions {
    include_index: false,
    include_underline: false,
    include_file_headers: false,
};

/// `formatPatch(patch, headerOptions)` for a single patch.
pub fn format_patch(patch: &StructuredPatch, header_options: HeaderOptions) -> String {
    let mut ret: Vec<String> = Vec::new();
    if header_options.include_index && patch.old_file_name == patch.new_file_name {
        ret.push(format!("Index: {}", patch.old_file_name));
    }
    if header_options.include_underline {
        ret.push("===================================================================".to_string());
    }
    if header_options.include_file_headers {
        ret.push(format!(
            "--- {}{}",
            patch.old_file_name,
            patch
                .old_header
                .as_ref()
                .map(|h| format!("\t{h}"))
                .unwrap_or_default()
        ));
        ret.push(format!(
            "+++ {}{}",
            patch.new_file_name,
            patch
                .new_header
                .as_ref()
                .map(|h| format!("\t{h}"))
                .unwrap_or_default()
        ));
    }
    for hunk in &patch.hunks {
        // Unified diff quirk: when a side has no lines, its start is one lower.
        let old_start = if hunk.old_lines == 0 {
            hunk.old_start - 1
        } else {
            hunk.old_start
        };
        let new_start = if hunk.new_lines == 0 {
            hunk.new_start - 1
        } else {
            hunk.new_start
        };
        ret.push(format!(
            "@@ -{},{} +{},{} @@",
            old_start, hunk.old_lines, new_start, hunk.new_lines
        ));
        ret.extend(hunk.lines.iter().cloned());
    }
    format!("{}\n", ret.join("\n"))
}

/// `createTwoFilesPatch(oldFileName, newFileName, oldStr, newStr, oldHeader, newHeader, { context, headerOptions })`.
#[allow(clippy::too_many_arguments)]
pub fn create_two_files_patch(
    old_file_name: &str,
    new_file_name: &str,
    old_str: &str,
    new_str: &str,
    old_header: Option<&str>,
    new_header: Option<&str>,
    context: usize,
    header_options: HeaderOptions,
) -> String {
    let patch = structured_patch(
        old_file_name,
        new_file_name,
        old_str,
        new_str,
        old_header,
        new_header,
        context,
    );
    format_patch(&patch, header_options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(changes: &[Change]) -> Vec<(String, bool, bool, usize)> {
        changes
            .iter()
            .map(|c| (c.value.clone(), c.added, c.removed, c.count))
            .collect()
    }

    #[test]
    fn diff_lines_matches_jsdiff() {
        // Expected values produced by diff@8.0.4.
        let changes = diff_lines("a\nb\nc\n", "a\nx\nc\nd");
        assert_eq!(
            summary(&changes),
            vec![
                ("a\n".to_string(), false, false, 1),
                ("b\n".to_string(), false, true, 1),
                ("x\n".to_string(), true, false, 1),
                ("c\n".to_string(), false, false, 1),
                ("d".to_string(), true, false, 1),
            ]
        );
        assert_eq!(diff_lines("", ""), vec![]);
        assert_eq!(
            summary(&diff_lines("", "a")),
            vec![("a".to_string(), true, false, 1)]
        );
        assert_eq!(
            tokenize_lines("a\r\nb\n\nc"),
            vec!["a\r\n", "b\n", "\n", "c"]
        );
    }

    #[test]
    fn two_files_patch_matches_jsdiff() {
        let patch = create_two_files_patch(
            "f.txt",
            "f.txt",
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n",
            "1\n2\n3\nfour\n5\n6\n7\n8\n9\n10\n11\ntwelve",
            None,
            None,
            4,
            FILE_HEADERS_ONLY,
        );
        assert_eq!(
            patch,
            "--- f.txt\n+++ f.txt\n@@ -1,12 +1,12 @@\n 1\n 2\n 3\n-4\n+four\n 5\n 6\n 7\n 8\n 9\n 10\n 11\n-12\n+twelve\n\\ No newline at end of file\n"
        );
        let empty =
            create_two_files_patch("a", "a", "x\n", "x\n", None, None, 4, FILE_HEADERS_ONLY);
        assert_eq!(empty, "--- a\n+++ a\n");
    }
}
