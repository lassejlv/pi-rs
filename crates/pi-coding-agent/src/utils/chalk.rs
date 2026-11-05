//! Minimal port of the `chalk` styling used by the CLI (`chalk.bold`, `chalk.red`, ...).
//!
//! chalk's default instance decides its color level from stdout using its vendored
//! `supports-color`; the same detection is reproduced here so console output matches
//! (plain text when stdout is not a terminal, `FORCE_COLOR` overrides, `TERM=dumb`, CI
//! detection). Only the basic 16-color styles are used by the CLI, so any level >= 1
//! produces the same escape codes.

use std::io::IsTerminal;
use std::sync::OnceLock;

static LEVEL: OnceLock<u8> = OnceLock::new();

fn env_force_color() -> Option<u8> {
    let value = std::env::var("FORCE_COLOR").ok()?;
    if value == "true" || value.is_empty() {
        return Some(1);
    }
    if value == "false" {
        return Some(0);
    }
    // `Math.min(Number.parseInt(value, 10), 3)`; NaN propagates as "no force".
    let digits: String = value
        .trim_start()
        .chars()
        .enumerate()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+')))
        .map(|(_, c)| c)
        .collect();
    let parsed: i64 = digits.parse().ok()?;
    Some(parsed.clamp(0, 3) as u8)
}

fn has_flag(flag: &str) -> bool {
    let position = |needle: &str| crate::cli::setup::process_args().iter().position(|a| a == needle);
    let prefix = if flag.starts_with('-') {
        ""
    } else if flag.len() == 1 {
        "-"
    } else {
        "--"
    };
    let Some(index) = position(&format!("{prefix}{flag}")) else {
        return false;
    };
    match position("--") {
        Some(terminator) => index < terminator,
        None => true,
    }
}

/// `supports-color` level detection for a stream.
pub fn detect_color_level(stream_is_tty: bool) -> u8 {
    let env = |key: &str| std::env::var(key).ok();
    let has = |key: &str| std::env::var_os(key).is_some();

    let mut flag_force_color: Option<u8> = None;
    if has_flag("no-color")
        || has_flag("no-colors")
        || has_flag("color=false")
        || has_flag("color=never")
    {
        flag_force_color = Some(0);
    } else if has_flag("color")
        || has_flag("colors")
        || has_flag("color=true")
        || has_flag("color=always")
    {
        flag_force_color = Some(1);
    }
    if let Some(force) = env_force_color() {
        flag_force_color = Some(force);
    }
    let force_color = flag_force_color;
    if force_color == Some(0) {
        return 0;
    }
    if has_flag("color=16m") || has_flag("color=full") || has_flag("color=truecolor") {
        return 3;
    }
    if has_flag("color=256") {
        return 2;
    }
    if has("TF_BUILD") && has("AGENT_NAME") {
        return 1;
    }
    if !stream_is_tty && force_color.is_none() {
        return 0;
    }
    let min = force_color.unwrap_or(0);
    if env("TERM").as_deref() == Some("dumb") {
        return min;
    }
    if cfg!(windows) {
        return 1;
    }
    if has("CI") {
        if ["GITHUB_ACTIONS", "GITEA_ACTIONS", "CIRCLECI"].iter().any(|k| has(k)) {
            return 3;
        }
        if ["TRAVIS", "APPVEYOR", "GITLAB_CI", "BUILDKITE", "DRONE"].iter().any(|k| has(k))
            || env("CI_NAME").as_deref() == Some("codeship")
        {
            return 1;
        }
        return min;
    }
    if let Some(version) = env("TEAMCITY_VERSION") {
        let re = regex::Regex::new(r"^(9\.(0*[1-9]\d*)\.|\d{2,}\.)").expect("valid regex");
        return if re.is_match(&version) { 1 } else { 0 };
    }
    if env("COLORTERM").as_deref() == Some("truecolor") {
        return 3;
    }
    let term = env("TERM").unwrap_or_default();
    if term == "xterm-kitty" || term == "xterm-ghostty" || term == "wezterm" {
        return 3;
    }
    if let Some(program) = env("TERM_PROGRAM") {
        let version = env("TERM_PROGRAM_VERSION").unwrap_or_default();
        let major: i64 = version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
        match program.as_str() {
            "iTerm.app" => return if major >= 3 { 3 } else { 2 },
            "Apple_Terminal" => return 2,
            _ => {}
        }
    }
    let lower = term.to_lowercase();
    if lower.ends_with("-256") || lower.ends_with("-256color") {
        return 2;
    }
    let basic = regex::Regex::new(r"(?i)^screen|^xterm|^vt100|^vt220|^rxvt|color|ansi|cygwin|linux")
        .expect("valid regex");
    if basic.is_match(&term) {
        return 1;
    }
    if has("COLORTERM") {
        return 1;
    }
    min
}

/// Color level of chalk's default instance (based on stdout).
pub fn level() -> u8 {
    *LEVEL.get_or_init(|| detect_color_level(std::io::stdout().is_terminal()))
}

fn style(text: &str, open: &str, close: &str) -> String {
    if level() == 0 || text.is_empty() {
        return text.to_string();
    }
    // chalk re-opens the style after nested closes and around line breaks.
    let body = if text.contains('\x1b') {
        text.replace(close, &format!("{close}{open}"))
    } else {
        text.to_string()
    };
    let mut out = String::with_capacity(body.len() + 16);
    out.push_str(open);
    let mut rest = body.as_str();
    while let Some(index) = rest.find('\n') {
        let got_cr = index > 0 && rest.as_bytes()[index - 1] == b'\r';
        out.push_str(&rest[..if got_cr { index - 1 } else { index }]);
        out.push_str(close);
        out.push_str(if got_cr { "\r\n" } else { "\n" });
        out.push_str(open);
        rest = &rest[index + 1..];
    }
    out.push_str(rest);
    out.push_str(close);
    out
}

pub fn bold(text: &str) -> String {
    style(text, "\x1b[1m", "\x1b[22m")
}

pub fn dim(text: &str) -> String {
    style(text, "\x1b[2m", "\x1b[22m")
}

pub fn red(text: &str) -> String {
    style(text, "\x1b[31m", "\x1b[39m")
}

pub fn green(text: &str) -> String {
    style(text, "\x1b[32m", "\x1b[39m")
}

pub fn yellow(text: &str) -> String {
    style(text, "\x1b[33m", "\x1b[39m")
}

pub fn blue(text: &str) -> String {
    style(text, "\x1b[34m", "\x1b[39m")
}

pub fn cyan(text: &str) -> String {
    style(text, "\x1b[36m", "\x1b[39m")
}

pub fn gray(text: &str) -> String {
    style(text, "\x1b[90m", "\x1b[39m")
}

pub fn italic(text: &str) -> String {
    style(text, "\x1b[3m", "\x1b[23m")
}

pub fn underline(text: &str) -> String {
    style(text, "\x1b[4m", "\x1b[24m")
}

pub fn inverse(text: &str) -> String {
    style(text, "\x1b[7m", "\x1b[27m")
}

pub fn strikethrough(text: &str) -> String {
    style(text, "\x1b[9m", "\x1b[29m")
}
