//! Complete-line tool output bounds; streaming tools use Chord's OutputBuffer.
use crate::JsonString;
use pi_chord::output::utf8_byte_length;
use serde::Serialize;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_LINES: usize = 2000;
#[derive(Clone, Copy, Debug, Default)]
pub struct TruncationOptions {
    pub max_bytes: Option<f64>,
    pub max_lines: Option<f64>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    pub content: JsonString,
    pub truncated: bool,
    pub truncated_by: Option<&'static str>,
    pub total_lines: f64,
    pub total_bytes: f64,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: f64,
    pub max_bytes: f64,
}
pub fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() || value.abs() >= 1e21 {
        return pi_json::number_to_string(value);
    }
    let value = if value == 0.0 { 0.0 } else { value };
    let wide = format!("{:.*}", digits + 40, value.abs());
    let dot = wide.find('.').unwrap_or(wide.len());
    let tail = &wide[dot + 1 + digits..];
    if !(tail.starts_with('5') && tail[1..].bytes().all(|b| b == b'0')) {
        return format!("{value:.digits$}");
    }
    let factor = 10f64.powi(digits as i32);
    let up = ((value.abs() * factor).trunc() + 1.0) / factor;
    let signed = if value < 0.0 { -up } else { up };
    format!("{signed:.digits$}")
}
pub fn format_size(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!("{}B", pi_json::number_to_string(bytes))
    } else if bytes < 1024.0 * 1024.0 {
        format!("{}KB", to_fixed(bytes / 1024.0, 1))
    } else {
        format!("{}MB", to_fixed(bytes / (1024.0 * 1024.0), 1))
    }
}
fn lines(text: &JsonString) -> Vec<&[u16]> {
    if text.units().is_empty() {
        return vec![];
    }
    let mut lines = text.units().split(|&u| u == 10).collect::<Vec<_>>();
    if text.units().last() == Some(&10) {
        lines.pop();
    }
    lines
}
pub fn truncate_head(content: &JsonString, options: TruncationOptions) -> TruncationResult {
    truncate_head_of(
        content,
        lines(content).len() as f64,
        utf8_byte_length(content) as f64,
        options,
    )
}
pub fn truncate_head_of(
    prefix: &JsonString,
    total_lines: f64,
    total_bytes: f64,
    options: TruncationOptions,
) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES as f64);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES as f64);
    let mut result = TruncationResult {
        content: prefix.clone(),
        truncated: false,
        truncated_by: None,
        total_lines,
        total_bytes,
        output_lines: total_lines as usize,
        output_bytes: total_bytes as usize,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    };
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return result;
    }
    let lines = lines(prefix);
    result.truncated = true;
    if lines.first().is_some_and(|line| {
        utf8_byte_length(&JsonString::from_utf16(line.to_vec())) as f64 > max_bytes
    }) {
        result.content = JsonString::default();
        result.truncated_by = Some("bytes");
        result.output_lines = 0;
        result.output_bytes = 0;
        result.first_line_exceeds_limit = true;
        return result;
    }
    let mut output = JsonString::default();
    let mut count = 0;
    let mut bytes = 0;
    let mut by = "lines";
    for line in lines {
        if (count as f64).partial_cmp(&max_lines) != Some(std::cmp::Ordering::Less) {
            break;
        }
        let line = JsonString::from_utf16(line.to_vec());
        let size = utf8_byte_length(&line) + usize::from(count > 0);
        if (bytes + size) as f64 > max_bytes {
            by = "bytes";
            break;
        }
        if count > 0 {
            output.append(&"\n".into());
        }
        output.append(&line);
        bytes += size;
        count += 1;
    }
    if by != "bytes" {
        by = if (count as f64) < total_lines {
            "lines"
        } else {
            "bytes"
        };
    }
    result.content = output;
    result.truncated_by = Some(by);
    result.output_lines = count;
    result.output_bytes = bytes;
    result
}
