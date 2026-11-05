//! Port of `packages/coding-agent/src/modes/interactive/theme/theme.ts`.
//!
//! Global state (TS module-level `let`s and the `globalThis` theme slot) lives in statics below:
//! the active theme, the registered themes, the terminal's reported colors, the theme JSON validator,
//! the change callback and the custom-theme file watcher.

use crate::config::{get_custom_themes_dir, get_themes_dir};
use crate::core::source_info::SourceInfo;
use crate::modes::interactive::theme::system_theme::{
    SystemThemeColorsColors, SystemThemeInput, generate_system_theme_colors, terminal_appearance,
};
use crate::modes::interactive::theme::theme_schema::{ThemeColorValue, ValidatedThemeJson};
use crate::modes::interactive::theme::theme_tokens::{THEME_TOKENS, ThemeTokenSlot};
use crate::utils::fs_watch::{FsWatcher, WatchEventType, close_watcher, watch_with_error_handler};
use crate::utils::syntax_highlight::{HighlightOptions, highlight, supports_language};
use crate::utils::text::strip_bom;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use pi_agent::types::ThinkingLevel;
use pi_tui::colors::{
    Color, ColorMixSpace, TextAttributes, background_ansi, color_to_hex, color_to_oklch, foreground_ansi,
    indexed_color, mix_colors, parse_color, rgb_color, style_text_with_ansi,
};
use pi_tui::colors::{ColorValue, TerminalColorMode};
use pi_tui::components::editor::EditorTheme;
use pi_tui::components::markdown::MarkdownTheme;
use pi_tui::components::select_list::SelectListTheme;
use pi_tui::components::settings_list::SettingsListTheme;
use pi_tui::terminal_colors::{RgbColor, TerminalColors};
use pi_tui::terminal_image::get_terminal_color_mode;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

pub use crate::modes::interactive::theme::system_theme::SYSTEM_THEME_NAME;
pub use crate::modes::interactive::theme::theme_schema::ValidatedThemeJson as ThemeJson;
pub use crate::modes::interactive::theme::theme_tokens::ThemeBg;
pub use crate::modes::interactive::theme::theme_tokens::ThemeColor;
pub use crate::modes::interactive::theme::theme_tokens::ThemeToken;

/// Built-in dark theme JSON (embedded; TS reads `theme/dark.json` next to the package).
pub const BUILTIN_DARK_JSON: &str = include_str!("dark.json");
/// Built-in light theme JSON.
pub const BUILTIN_LIGHT_JSON: &str = include_str!("light.json");

/// A foreground for [`ThemeStyle`]: a token or a concrete color.
#[derive(Debug, Clone)]
pub enum ThemeStyleFg {
    ThemeColor(ThemeColor),
    Color(Color),
}

/// A background for [`ThemeStyle`]: a token or a concrete color.
#[derive(Debug, Clone)]
pub enum ThemeStyleBg {
    ThemeBg(ThemeBg),
    Color(Color),
}

/// Tokens are only accepted in their own slot, because "" (terminal default) means the default foreground
/// or background depending on the slot. Use `theme.colors[token]` to use a token's color in the other slot.
// TS: extends TextAttributes (inherited fields inlined)
#[derive(Debug, Clone, Default)]
pub struct ThemeStyle {
    pub bold: Option<bool>,
    pub dim: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub inverse: Option<bool>,
    pub strikethrough: Option<bool>,
    pub fg: Option<ThemeStyleFg>,
    pub bg: Option<ThemeStyleBg>,
}

pub type ThemeJsonValidator = Arc<dyn Fn(String, Value) -> anyhow::Result<ValidatedThemeJson> + Send + Sync>;

static THEME_JSON_VALIDATOR: RwLock<Option<ThemeJsonValidator>> = RwLock::new(None);

/// Install full theme validation. Without it, documents are accepted as-is, which is what built-in
/// themes already do.
pub fn set_theme_json_validator(validator: &ThemeJsonValidator) {
    *THEME_JSON_VALIDATOR.write() = Some(validator.clone());
}

// ============================================================================
// Color Utilities
// ============================================================================

static OK_FUNCTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^ok(lch|hsl)\(").unwrap());
static OKHSL_FUNCTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^okhsl\(").unwrap());

fn resolve_var_refs(
    value: &ThemeColorValue,
    vars: &IndexMap<String, ThemeColorValue>,
    visited: &mut HashSet<String>,
) -> anyhow::Result<ThemeColorValue> {
    let name = match value {
        ThemeColorValue::Index(_) => return Ok(value.clone()),
        ThemeColorValue::String(s) => {
            if s.is_empty() || s.starts_with('#') || OK_FUNCTION.is_match(s) {
                return Ok(value.clone());
            }
            s
        }
    };
    if visited.contains(name) {
        anyhow::bail!("Circular variable reference detected: {name}");
    }
    let Some(next) = vars.get(name) else {
        anyhow::bail!("Variable reference not found: {name}");
    };
    visited.insert(name.clone());
    resolve_var_refs(next, vars, visited)
}

/// Resolve `colors` (JSON key order kept) to token values. Unknown keys are ignored.
fn resolve_theme_colors(
    colors: &IndexMap<String, ThemeColorValue>,
    vars: Option<&IndexMap<String, ThemeColorValue>>,
) -> anyhow::Result<IndexMap<ThemeToken, ThemeColorValue>> {
    let empty = IndexMap::new();
    let vars = vars.unwrap_or(&empty);
    let mut resolved = IndexMap::new();
    for (key, value) in colors {
        let value = resolve_var_refs(value, vars, &mut HashSet::new())?;
        if let Some(token) = ThemeToken::parse(key) {
            resolved.insert(token, value);
        }
    }
    Ok(resolved)
}

fn with_theme_token_fallbacks<V: Clone>(mut values: IndexMap<ThemeToken, V>) -> anyhow::Result<IndexMap<ThemeToken, V>> {
    for (token, descriptor) in THEME_TOKENS {
        let Some(fallback) = descriptor.fallback else { continue };
        if values.contains_key(token) {
            continue;
        }
        let Some(fallback_value) = values.get(&fallback).cloned() else {
            anyhow::bail!("Theme token {} has unresolved fallback {}", token.as_str(), fallback.as_str());
        };
        values.insert(*token, fallback_value);
    }
    Ok(values)
}

// ============================================================================
// Appearance & Terminal Default Colors
// ============================================================================

/// The background a theme is designed for.
pub type ThemeAppearance = TerminalTheme;

struct TerminalState {
    /// The terminal's reported colors. Replaced (never mutated) on update, so themes can cache resolved
    /// colors by identity.
    colors: Arc<TerminalColors>,
    /// While the terminal color query is in flight, the system theme renders in grayscale.
    pending: bool,
    /// The terminal's last light/dark report (mode 2031).
    scheme: Option<TerminalTheme>,
}

static TERMINAL: LazyLock<Mutex<TerminalState>> = LazyLock::new(|| {
    Mutex::new(TerminalState { colors: Arc::new(TerminalColors::default()), pending: false, scheme: None })
});

fn terminal_colors() -> Arc<TerminalColors> {
    TERMINAL.lock().colors.clone()
}

/// Record the terminal's reported colors. Themes use the default colors for tokens set to "" (terminal
/// default); the system theme is generated from all of them. Ends the pending state.
pub fn set_terminal_colors(colors: Arc<TerminalColors>) {
    let mut state = TERMINAL.lock();
    state.colors = Arc::new((*colors).clone());
    state.pending = false;
}

/// Record the terminal's light/dark report, the fallback for terminals that do not report their background.
pub fn set_terminal_color_scheme(scheme: Option<&TerminalTheme>) {
    TERMINAL.lock().scheme = scheme.copied();
}

/// Render the system theme in grayscale until `setTerminalColors()` reports the terminal's colors.
pub fn mark_terminal_colors_pending() {
    TERMINAL.lock().pending = true;
}

/// Assumed terminal default colors when the terminal does not report them.
fn guessed_default_colors(appearance: ThemeAppearance) -> (Color, Color) {
    match appearance {
        TerminalTheme::Dark => (parse_color("#e5e5e7").unwrap(), parse_color("#000000").unwrap()),
        TerminalTheme::Light => (parse_color("#000000").unwrap(), parse_color("#ffffff").unwrap()),
    }
}

fn average_lightness(colors: &[Color]) -> Option<f64> {
    // Palette colors 0-15 follow the user's terminal palette, so they say nothing about the theme.
    let fixed: Vec<&Color> =
        colors.iter().filter(|c| !matches!(c, Color::Indexed { index } if *index < 16)).collect();
    if fixed.is_empty() {
        return None;
    }
    Some(fixed.iter().map(|c| color_to_oklch(c).l).sum::<f64>() / fixed.len() as f64)
}

/// Detect the background a theme is designed for from the lightness of its own colors.
fn detect_appearance(foregrounds: &[Color], backgrounds: &[Color]) -> Option<ThemeAppearance> {
    let fg = average_lightness(foregrounds);
    let bg = average_lightness(backgrounds);
    match (fg, bg) {
        (Some(fg), Some(bg)) => Some(if bg < fg { TerminalTheme::Dark } else { TerminalTheme::Light }),
        (None, Some(bg)) => Some(if bg < 0.5 { TerminalTheme::Dark } else { TerminalTheme::Light }),
        (Some(fg), None) => Some(if fg > 0.5 { TerminalTheme::Dark } else { TerminalTheme::Light }),
        (None, None) => None,
    }
}

fn rgb_to_color(rgb: &RgbColor) -> Color {
    rgb_color(rgb.r.clamp(0.0, 255.0), rgb.g.clamp(0.0, 255.0), rgb.b.clamp(0.0, 255.0))
        .unwrap_or(Color::Rgb { r: 0.0, g: 0.0, b: 0.0 })
}

fn parse_color_value(value: &ThemeColorValue) -> anyhow::Result<Color> {
    let parsed = match value {
        ThemeColorValue::Index(i) => parse_color(ColorValue::Index(*i)),
        ThemeColorValue::String(s) => parse_color(ColorValue::Str(s.clone())),
    };
    parsed.map_err(|e| anyhow::anyhow!(e))
}

// ============================================================================
// Theme Class
// ============================================================================

/// Foreground token values (`Pick<ThemeColorValues<string | number>, ThemeColor>`), in theme order.
pub type ThemeFgColors = IndexMap<ThemeToken, ThemeColorValue>;
/// Background token values (`Pick<ThemeColorValues<string | number>, ThemeBg>`), in theme order.
pub type ThemeBgColors = IndexMap<ThemeToken, ThemeColorValue>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_info: Option<SourceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appearance: Option<ThemeAppearance>,
    /// Foreground tokens to render faint (SGR 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<Vec<ThemeColor>>,
}

pub struct Theme {
    name: Option<String>,
    source_path: Option<String>,
    source_info: Mutex<Option<SourceInfo>>,
    mode: TerminalColorMode,
    // Precomputed escape sequences keep fg()/bg() on the render hot path to a lookup and concat.
    fg_ansi: HashMap<ThemeToken, String>,
    bg_ansi: HashMap<ThemeToken, String>,
    // Tokens set to "" have no color of their own; `colors` fills them from the terminal defaults.
    concrete_colors: IndexMap<ThemeToken, Color>,
    default_foreground_tokens: Vec<ThemeToken>,
    default_background_tokens: Vec<ThemeToken>,
    // Foreground tokens rendered faint (SGR 2) on top of their color.
    dim_tokens: Vec<ThemeColor>,
    own_appearance: Option<ThemeAppearance>,
    resolved_colors: Mutex<Option<(Arc<TerminalColors>, Arc<IndexMap<ThemeToken, Color>>)>>,
}

impl std::fmt::Debug for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Theme").field("name", &self.name).field("source_path", &self.source_path).finish()
    }
}

impl Theme {
    /// Build a theme. Panics on an invalid color value; use [`Theme::try_new`] for fallible construction.
    pub fn new(
        fg_colors: &ThemeFgColors,
        bg_colors: &ThemeBgColors,
        mode: TerminalColorMode,
        options: Option<&ThemeOptions>,
    ) -> Self {
        Self::try_new(fg_colors, bg_colors, mode, options).expect("invalid theme colors")
    }

    pub fn try_new(
        fg_colors: &ThemeFgColors,
        bg_colors: &ThemeBgColors,
        mode: TerminalColorMode,
        options: Option<&ThemeOptions>,
    ) -> anyhow::Result<Self> {
        let default_options = ThemeOptions::default();
        let options = options.unwrap_or(&default_options);
        let mut merged: IndexMap<ThemeToken, ThemeColorValue> = fg_colors.clone();
        for (k, v) in bg_colors {
            merged.insert(*k, v.clone());
        }
        let (foregrounds, backgrounds) = split_theme_colors(with_theme_token_fallbacks(merged)?);

        let mut theme = Theme {
            name: options.name.clone(),
            source_path: options.source_path.clone(),
            source_info: Mutex::new(options.source_info.clone()),
            mode,
            fg_ansi: HashMap::new(),
            bg_ansi: HashMap::new(),
            concrete_colors: IndexMap::new(),
            default_foreground_tokens: Vec::new(),
            default_background_tokens: Vec::new(),
            dim_tokens: options.dim.clone().unwrap_or_default(),
            own_appearance: None,
            resolved_colors: Mutex::new(None),
        };
        let mut concrete_foregrounds = Vec::new();
        let mut concrete_backgrounds = Vec::new();
        for (token, value, is_background) in foregrounds
            .iter()
            .map(|(t, v)| (t, v, false))
            .chain(backgrounds.iter().map(|(t, v)| (t, v, true)))
        {
            let ansi = if matches!(value, ThemeColorValue::String(s) if s.is_empty()) {
                if is_background {
                    theme.default_background_tokens.push(*token);
                    "\x1b[49m".to_string()
                } else {
                    theme.default_foreground_tokens.push(*token);
                    "\x1b[39m".to_string()
                }
            } else {
                let color = parse_color_value(value)?;
                theme.concrete_colors.insert(*token, color);
                if is_background {
                    concrete_backgrounds.push(color);
                    background_ansi(&color, mode)
                } else {
                    concrete_foregrounds.push(color);
                    foreground_ansi(&color, mode)
                }
            };
            if is_background {
                theme.bg_ansi.insert(*token, ansi);
            } else {
                theme.fg_ansi.insert(*token, ansi);
            }
        }
        theme.own_appearance =
            options.appearance.or_else(|| detect_appearance(&concrete_foregrounds, &concrete_backgrounds));
        Ok(theme)
    }

    pub fn name(&self) -> Option<String> {
        self.name.clone()
    }

    pub fn source_path(&self) -> Option<String> {
        self.source_path.clone()
    }

    pub fn source_info(&self) -> Option<SourceInfo> {
        self.source_info.lock().clone()
    }

    pub fn set_source_info(&self, value: Option<SourceInfo>) {
        *self.source_info.lock() = value;
    }

    /// The background the theme is designed for: declared in the theme JSON, detected from its colors,
    /// or, for themes without usable colors, the terminal's appearance.
    pub fn appearance(&self) -> ThemeAppearance {
        self.own_appearance.unwrap_or_else(get_terminal_theme)
    }

    /// Concrete colors for all tokens. Tokens set to "" (terminal default) use the terminal's reported
    /// default colors, or a guess based on `appearance` when the terminal did not report them. Faint
    /// tokens are approximated by mixing their color toward the background.
    pub fn colors(&self) -> IndexMap<ThemeToken, Color> {
        (*self.colors_arc()).clone()
    }

    /// The color of one token (see [`Theme::colors`]).
    pub fn color(&self, token: ThemeToken) -> Option<Color> {
        self.colors_arc().get(&token).copied()
    }

    fn colors_arc(&self) -> Arc<IndexMap<ThemeToken, Color>> {
        let terminal = terminal_colors();
        if let Some((cached_terminal, colors)) = &*self.resolved_colors.lock()
            && Arc::ptr_eq(cached_terminal, &terminal)
        {
            return colors.clone();
        }
        let (guess_fg, guess_bg) = guessed_default_colors(self.appearance());
        let foreground = terminal.foreground.as_ref().map(rgb_to_color).unwrap_or(guess_fg);
        let background = terminal.background.as_ref().map(rgb_to_color).unwrap_or(guess_bg);
        let mut colors = self.concrete_colors.clone();
        for token in &self.default_foreground_tokens {
            colors.insert(*token, foreground);
        }
        for token in &self.default_background_tokens {
            colors.insert(*token, background);
        }
        for token in &self.dim_tokens {
            if let Some(color) = colors.get(&token.token()).copied()
                && let Ok(mixed) = mix_colors(&color, &background, 0.4, ColorMixSpace::Oklch)
            {
                colors.insert(token.token(), mixed);
            }
        }
        let colors = Arc::new(colors);
        *self.resolved_colors.lock() = Some((terminal, colors.clone()));
        colors
    }

    fn is_dim(&self, color: &ThemeColor) -> bool {
        self.dim_tokens.contains(color)
    }

    fn token_fg_ansi(&self, color: &ThemeColor) -> &str {
        self.fg_ansi.get(&color.token()).map(String::as_str).unwrap_or("")
    }

    fn token_bg_ansi(&self, color: &ThemeBg) -> &str {
        self.bg_ansi.get(&color.token()).map(String::as_str).unwrap_or("")
    }

    pub fn style(&self, text: &str, options: &ThemeStyle) -> String {
        let mut attrs = TextAttributes {
            bold: options.bold.unwrap_or(false),
            dim: options.dim.unwrap_or(false),
            italic: options.italic.unwrap_or(false),
            underline: options.underline.unwrap_or(false),
            inverse: options.inverse.unwrap_or(false),
            strikethrough: options.strikethrough.unwrap_or(false),
        };
        if let Some(ThemeStyleFg::ThemeColor(color)) = &options.fg
            && self.is_dim(color)
        {
            attrs.dim = true;
        }
        let fg = options.fg.as_ref().map(|fg| match fg {
            ThemeStyleFg::ThemeColor(token) => self.token_fg_ansi(token).to_string(),
            ThemeStyleFg::Color(color) => foreground_ansi(color, self.mode),
        });
        let bg = options.bg.as_ref().map(|bg| match bg {
            ThemeStyleBg::ThemeBg(token) => self.token_bg_ansi(token).to_string(),
            ThemeStyleBg::Color(color) => background_ansi(color, self.mode),
        });
        style_text_with_ansi(text, fg.as_deref(), bg.as_deref(), &attrs)
    }

    pub fn fg(&self, color: &ThemeColor, text: &str) -> String {
        let ansi = self.token_fg_ansi(color);
        if self.is_dim(color) {
            return format!("{ansi}\x1b[2m{text}\x1b[22;39m");
        }
        format!("{ansi}{text}\x1b[39m")
    }

    pub fn bg(&self, color: &ThemeBg, text: &str) -> String {
        format!("{}{text}\x1b[49m", self.token_bg_ansi(color))
    }

    pub fn bold(&self, text: &str) -> String {
        crate::utils::chalk::bold(text)
    }

    pub fn italic(&self, text: &str) -> String {
        crate::utils::chalk::italic(text)
    }

    pub fn underline(&self, text: &str) -> String {
        crate::utils::chalk::underline(text)
    }

    pub fn inverse(&self, text: &str) -> String {
        crate::utils::chalk::inverse(text)
    }

    pub fn strikethrough(&self, text: &str) -> String {
        crate::utils::chalk::strikethrough(text)
    }

    /// Opening escape sequence for a foreground token. Faint tokens include SGR 2, which `\x1b[22m` closes.
    pub fn get_fg_ansi(&self, color: &ThemeColor) -> String {
        let ansi = self.token_fg_ansi(color);
        if self.is_dim(color) { format!("{ansi}\x1b[2m") } else { ansi.to_string() }
    }

    pub fn get_bg_ansi(&self, color: &ThemeBg) -> String {
        self.token_bg_ansi(color).to_string()
    }

    pub fn get_color_mode(&self) -> TerminalColorMode {
        self.mode
    }

    pub fn get_thinking_border_color(&self, level: ThinkingLevel) -> Arc<dyn Fn(String) -> String + Send + Sync> {
        let color = match level {
            ThinkingLevel::Off => ThemeColor::ThinkingOff,
            ThinkingLevel::Minimal => ThemeColor::ThinkingMinimal,
            ThinkingLevel::Low => ThemeColor::ThinkingLow,
            ThinkingLevel::Medium => ThemeColor::ThinkingMedium,
            ThinkingLevel::High => ThemeColor::ThinkingHigh,
            ThinkingLevel::Xhigh => ThemeColor::ThinkingXhigh,
            ThinkingLevel::Max => ThemeColor::ThinkingMax,
        };
        let fg = self.token_fg_ansi(&color).to_string();
        let dim = self.is_dim(&color);
        Arc::new(move |s: String| {
            if dim { format!("{fg}\x1b[2m{s}\x1b[22;39m") } else { format!("{fg}{s}\x1b[39m") }
        })
    }

    pub fn get_bash_mode_border_color(&self) -> Arc<dyn Fn(String) -> String + Send + Sync> {
        let fg = self.token_fg_ansi(&ThemeColor::BashMode).to_string();
        let dim = self.is_dim(&ThemeColor::BashMode);
        Arc::new(move |s: String| {
            if dim { format!("{fg}\x1b[2m{s}\x1b[22;39m") } else { format!("{fg}{s}\x1b[39m") }
        })
    }
}

// ============================================================================
// Theme Loading
// ============================================================================

static BUILTIN_THEMES: LazyLock<IndexMap<String, ThemeJson>> = LazyLock::new(|| {
    let mut themes = IndexMap::new();
    themes.insert(
        "dark".to_string(),
        serde_json::from_str(&strip_bom(BUILTIN_DARK_JSON)).expect("built-in dark theme is valid"),
    );
    themes.insert(
        "light".to_string(),
        serde_json::from_str(&strip_bom(BUILTIN_LIGHT_JSON)).expect("built-in light theme is valid"),
    );
    themes
});

fn get_builtin_themes() -> &'static IndexMap<String, ThemeJson> {
    &BUILTIN_THEMES
}

pub fn get_available_themes() -> Vec<String> {
    get_available_themes_with_paths().into_iter().map(|t| t.name).collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

fn js_locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    pi_tui::collation::locale_compare(a, b)
}

pub fn get_available_themes_with_paths() -> Vec<ThemeInfo> {
    let themes_dir = get_themes_dir();
    let mut result: Vec<ThemeInfo> = Vec::new();
    let mut seen = HashSet::new();
    let mut add_theme = |info: ThemeInfo, result: &mut Vec<ThemeInfo>| {
        if seen.insert(info.name.clone()) {
            result.push(info);
        }
    };

    // Built-in themes. The system theme is generated, so it has no file.
    add_theme(ThemeInfo { name: SYSTEM_THEME_NAME.to_string(), path: None }, &mut result);
    for name in get_builtin_themes().keys() {
        let path = Path::new(&themes_dir).join(format!("{name}.json")).to_string_lossy().into_owned();
        add_theme(ThemeInfo { name: name.clone(), path: Some(path) }, &mut result);
    }

    // Custom themes
    for info in get_custom_theme_infos() {
        add_theme(info, &mut result);
    }

    let registered: Vec<(String, Option<String>)> =
        REGISTERED_THEMES.lock().iter().map(|(name, theme)| (name.clone(), theme.source_path())).collect();
    for (name, path) in registered {
        add_theme(ThemeInfo { name, path }, &mut result);
    }

    // The system theme comes first: it is the default and adapts to every terminal.
    result.sort_by(|a, b| {
        if a.name == SYSTEM_THEME_NAME {
            std::cmp::Ordering::Less
        } else if b.name == SYSTEM_THEME_NAME {
            std::cmp::Ordering::Greater
        } else {
            js_locale_compare(&a.name, &b.name)
        }
    });
    result
}

fn get_custom_theme_infos() -> Vec<ThemeInfo> {
    let custom_themes_dir = get_custom_themes_dir();
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(&custom_themes_dir) else {
        return result;
    };
    let mut files: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    for file in files {
        if !file.ends_with(".json") {
            continue;
        }
        let theme_path = Path::new(&custom_themes_dir).join(&file).to_string_lossy().into_owned();
        // Invalid themes are ignored here; the resource loader reports them during normal startup/reload.
        if let Ok(theme) = load_theme_from_path(&theme_path, None)
            && let Some(name) = theme.name()
        {
            result.push(ThemeInfo { name, path: Some(theme_path) });
        }
    }
    result
}

fn assert_theme_name_is_valid(name: &str) -> anyhow::Result<()> {
    if name.contains('/') {
        anyhow::bail!(
            "Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
        );
    }
    Ok(())
}

fn parse_theme_json(label: &str, json: Value) -> anyhow::Result<ThemeJson> {
    let validator = THEME_JSON_VALIDATOR.read().clone();
    if let Some(validator) = validator {
        return validator(label.to_string(), json);
    }
    if !json.is_object() || json.get("colors").is_none() {
        anyhow::bail!("Invalid theme \"{label}\": expected an object with a \"colors\" map.");
    }
    // Documents are accepted as-is; a missing name is tolerated like in TS.
    let mut json = json;
    if json.get("name").is_none() {
        json["name"] = Value::String(String::new());
    }
    serde_json::from_value(json).map_err(|e| anyhow::anyhow!("Invalid theme \"{label}\": {e}"))
}

fn parse_theme_json_content(label: &str, content: &str) -> anyhow::Result<ThemeJson> {
    let json: Value = crate::utils::js_json::js_json_parse(&strip_bom(content))
        .map_err(|error| anyhow::anyhow!("Failed to parse theme {label}: SyntaxError: {error}"))?;
    parse_theme_json(label, json)
}

fn load_theme_json(name: &str) -> anyhow::Result<ThemeJson> {
    if let Some(theme) = get_builtin_themes().get(name) {
        return Ok(theme.clone());
    }
    let registered = REGISTERED_THEMES.lock().get(name).cloned();
    if let Some(registered) = registered {
        if let Some(source_path) = registered.source_path() {
            let content = std::fs::read_to_string(&source_path)?;
            return parse_theme_json_content(&source_path, &content);
        }
        anyhow::bail!("Theme \"{name}\" does not have a source path for export");
    }
    let theme_path = Path::new(&get_custom_themes_dir()).join(format!("{name}.json"));
    if !theme_path.exists() {
        anyhow::bail!("Theme not found: {name}");
    }
    let content = std::fs::read_to_string(&theme_path)?;
    parse_theme_json_content(name, &content)
}

fn split_theme_colors(
    colors: IndexMap<ThemeToken, ThemeColorValue>,
) -> (IndexMap<ThemeToken, ThemeColorValue>, IndexMap<ThemeToken, ThemeColorValue>) {
    let mut fg = IndexMap::new();
    let mut bg = IndexMap::new();
    for (token, value) in colors {
        if token.slot() == ThemeTokenSlot::Background {
            bg.insert(token, value);
        } else {
            fg.insert(token, value);
        }
    }
    (fg, bg)
}

fn create_theme(
    theme_json: &ThemeJson,
    mode: Option<TerminalColorMode>,
    source_path: Option<&str>,
) -> anyhow::Result<Theme> {
    let color_mode = mode.unwrap_or_else(|| get_terminal_color_mode(None));
    let resolved = with_theme_token_fallbacks(resolve_theme_colors(&theme_json.colors, theme_json.vars.as_ref())?)?;
    let (fg, bg) = split_theme_colors(resolved);
    let name = if theme_json.name.is_empty() { None } else { Some(theme_json.name.clone()) };
    Theme::try_new(
        &fg,
        &bg,
        color_mode,
        Some(&ThemeOptions {
            name,
            source_path: source_path.map(str::to_string),
            appearance: theme_json.appearance,
            ..Default::default()
        }),
    )
}

/// Generate the system theme from the terminal's reported colors (grayscale while they are pending).
fn create_system_theme(mode: Option<TerminalColorMode>) -> Theme {
    let (colors, pending) = {
        let state = TERMINAL.lock();
        (state.colors.clone(), state.pending)
    };
    let generated = generate_system_theme_colors(&SystemThemeInput {
        foreground: colors.foreground,
        background: colors.background,
        palette: colors.palette.clone(),
        saturation: Some(if pending { 0.0 } else { 1.0 }),
        appearance_hint: Some(get_terminal_theme()),
    });
    let values: IndexMap<ThemeToken, ThemeColorValue> = generated
        .colors
        .into_iter()
        .map(|(token, value)| {
            let value = match value {
                SystemThemeColorsColors::String(s) => ThemeColorValue::String(s),
                SystemThemeColorsColors::Number(n) => ThemeColorValue::Index(n),
            };
            (token, value)
        })
        .collect();
    let (fg, bg) = split_theme_colors(with_theme_token_fallbacks(values).expect("system theme tokens are complete"));
    Theme::new(
        &fg,
        &bg,
        mode.unwrap_or_else(|| get_terminal_color_mode(None)),
        Some(&ThemeOptions {
            name: Some(SYSTEM_THEME_NAME.to_string()),
            appearance: generated.appearance,
            dim: Some(generated.dim),
            ..Default::default()
        }),
    )
}

/// Load a theme file. Fails (TS: throws) when the file cannot be read or is not a valid theme.
pub fn load_theme_from_path(theme_path: &str, mode: Option<TerminalColorMode>) -> anyhow::Result<Arc<Theme>> {
    let content = std::fs::read_to_string(theme_path)
        .map_err(|e| anyhow::anyhow!("ENOENT: no such file or directory, open '{theme_path}' ({e})"))?;
    let theme_json = parse_theme_json_content(theme_path, &content)?;
    Ok(Arc::new(create_theme(&theme_json, mode, Some(theme_path))?))
}

fn load_theme(name: &str, mode: Option<TerminalColorMode>) -> anyhow::Result<Arc<Theme>> {
    // The system theme name is reserved: it takes precedence over custom themes of the same name.
    if name == SYSTEM_THEME_NAME {
        return Ok(Arc::new(create_system_theme(mode)));
    }
    let registered = REGISTERED_THEMES.lock().get(name).cloned();
    if let Some(registered) = registered {
        return Ok(registered);
    }
    let theme_json = load_theme_json(name)?;
    Ok(Arc::new(create_theme(&theme_json, mode, None)?))
}

pub fn get_theme_by_name(name: &str) -> Option<Arc<Theme>> {
    load_theme(name, None).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TerminalTheme {
    #[serde(rename = "dark")]
    Dark,
    #[serde(rename = "light")]
    Light,
}

impl TerminalTheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseAutoThemeSettingResult {
    pub light_theme: String,
    pub dark_theme: String,
}

pub fn parse_auto_theme_setting(theme_setting: Option<&str>) -> Option<ParseAutoThemeSettingResult> {
    let theme_setting = theme_setting.filter(|s| !s.is_empty())?;
    let slash_index = theme_setting.find('/')?;
    if theme_setting[slash_index + 1..].contains('/') {
        return None;
    }
    let light_theme = theme_setting[..slash_index].trim();
    let dark_theme = theme_setting[slash_index + 1..].trim();
    if light_theme.is_empty() || dark_theme.is_empty() {
        return None;
    }
    Some(ParseAutoThemeSettingResult { light_theme: light_theme.to_string(), dark_theme: dark_theme.to_string() })
}

pub fn resolve_theme_setting(theme_setting: Option<&str>, terminal_theme: &TerminalTheme) -> Option<String> {
    if let Some(auto) = parse_auto_theme_setting(theme_setting) {
        return Some(if *terminal_theme == TerminalTheme::Light { auto.light_theme } else { auto.dark_theme });
    }
    let setting = theme_setting?;
    if setting.contains('/') {
        return None;
    }
    Some(setting.to_string())
}

fn env_lookup(env: &Option<Value>, key: &str) -> Option<String> {
    match env {
        Some(Value::Object(map)) => map.get(key).and_then(Value::as_str).map(str::to_string),
        Some(_) => None,
        None => std::env::var(key).ok(),
    }
}

/// Dark or light from the `COLORFGBG` environment variable some terminals set, or None without a
/// usable background index. The value is `fg;bg` or `fg;xpm;bg` (rxvt), where a field is an ANSI color
/// index or `default` when the color is not in the palette. Classified by index like Vim does: 0-6 and 8
/// (bright black, e.g. Solarized Dark's background) are dark, 7 and 9-15 are light.
///
/// `env` is a JSON object of environment variables; `None` reads the process environment.
pub fn detect_color_fg_bg_theme(env: Option<Value>) -> Option<TerminalTheme> {
    let value = env_lookup(&env, "COLORFGBG")?;
    let bg = value.split(';').next_back()?.trim().to_string();
    if bg.is_empty() || bg.len() > 2 || !bg.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let index: u32 = bg.parse().ok()?;
    if index > 15 {
        return None;
    }
    Some(if index <= 6 || index == 8 { TerminalTheme::Dark } else { TerminalTheme::Light })
}

/// Whether the terminal is dark or light. The background it renders decides, classified the same way the
/// system theme does. Without a reported background: the terminal's light/dark report, then COLORFGBG,
/// then dark.
pub fn detect_terminal_theme(
    colors: Option<Arc<TerminalColors>>,
    reported_scheme: Option<&TerminalTheme>,
    env: Option<Value>,
) -> TerminalTheme {
    if let Some(colors) = &colors
        && let Some(background) = colors.background
    {
        return terminal_appearance(background, colors.foreground);
    }
    reported_scheme.copied().or_else(|| detect_color_fg_bg_theme(env)).unwrap_or(TerminalTheme::Dark)
}

/// Whether the terminal is dark or light, from everything it reported so far. See `detectTerminalTheme()`.
pub fn get_terminal_theme() -> TerminalTheme {
    let (colors, scheme) = {
        let state = TERMINAL.lock();
        (state.colors.clone(), state.scheme)
    };
    detect_terminal_theme(Some(colors), scheme.as_ref(), None)
}

// ============================================================================
// Global Theme Instance
// ============================================================================

static GLOBAL_THEME: RwLock<Option<Arc<Theme>>> = RwLock::new(None);
static CURRENT_THEME_NAME: Mutex<Option<String>> = Mutex::new(None);
static ON_THEME_CHANGE: RwLock<Option<Arc<dyn Fn() + Send + Sync>>> = RwLock::new(None);
static REGISTERED_THEMES: LazyLock<Mutex<IndexMap<String, Arc<Theme>>>> = LazyLock::new(|| Mutex::new(IndexMap::new()));
static THEME_WATCHER: Mutex<Option<FsWatcher>> = Mutex::new(None);
/// Generation of the pending reload timer; bumping it cancels a scheduled reload.
static THEME_RELOAD_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The current global theme (TS: the exported `theme` proxy, which forwards to the active theme).
///
/// TS throws "Theme not initialized. Call initTheme() first." when no theme was set; the Rust port
/// initializes the system theme on first use instead.
pub fn theme() -> Arc<Theme> {
    if let Some(theme) = GLOBAL_THEME.read().clone() {
        return theme;
    }
    let theme = Arc::new(create_system_theme(None));
    let mut slot = GLOBAL_THEME.write();
    slot.get_or_insert(theme).clone()
}

/// The current global theme if one was initialized.
pub fn try_theme() -> Option<Arc<Theme>> {
    GLOBAL_THEME.read().clone()
}

fn set_global_theme(theme: Arc<Theme>) {
    *GLOBAL_THEME.write() = Some(theme);
}

fn notify_theme_change() {
    let callback = ON_THEME_CHANGE.read().clone();
    if let Some(callback) = callback {
        callback();
    }
}

pub fn set_registered_themes(themes: Vec<Arc<Theme>>) {
    let mut registered = REGISTERED_THEMES.lock();
    registered.clear();
    for theme in themes {
        if let Some(name) = theme.name() {
            if assert_theme_name_is_valid(&name).is_err() {
                // TS throws here; resource loading never registers such themes (the schema rejects them).
                continue;
            }
            registered.insert(name, theme);
        }
    }
}

pub fn init_theme(theme_name: Option<&str>, enable_watcher: Option<bool>) {
    let name = theme_name.unwrap_or(SYSTEM_THEME_NAME).to_string();
    *CURRENT_THEME_NAME.lock() = Some(name.clone());
    match load_theme(&name, None) {
        Ok(theme) => {
            set_global_theme(theme);
            if enable_watcher.unwrap_or(false) {
                start_theme_watcher();
            }
        }
        Err(_) => {
            // Theme is invalid - fall back to the system theme silently
            *CURRENT_THEME_NAME.lock() = Some(SYSTEM_THEME_NAME.to_string());
            set_global_theme(Arc::new(create_system_theme(None)));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetThemeResult {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn set_theme(name: &str, enable_watcher: Option<bool>) -> SetThemeResult {
    *CURRENT_THEME_NAME.lock() = Some(name.to_string());
    match load_theme(name, None) {
        Ok(theme) => {
            set_global_theme(theme);
            if enable_watcher.unwrap_or(false) {
                start_theme_watcher();
            }
            notify_theme_change();
            SetThemeResult { success: true, error: None }
        }
        Err(error) => {
            // Theme is invalid - fall back to the system theme
            *CURRENT_THEME_NAME.lock() = Some(SYSTEM_THEME_NAME.to_string());
            set_global_theme(Arc::new(create_system_theme(None)));
            SetThemeResult { success: false, error: Some(error.to_string()) }
        }
    }
}

pub fn set_theme_instance(theme_instance: Arc<Theme>) {
    set_global_theme(theme_instance);
    *CURRENT_THEME_NAME.lock() = Some("<in-memory>".to_string());
    stop_theme_watcher(); // Can't watch a direct instance
    notify_theme_change();
}

pub fn on_theme_change(callback: Arc<dyn Fn() + Send + Sync>) {
    *ON_THEME_CHANGE.write() = Some(callback);
}

fn start_theme_watcher() {
    stop_theme_watcher();

    // Only watch if it's a custom theme (not built-in)
    let Some(current) = CURRENT_THEME_NAME.lock().clone() else { return };
    if current == "dark" || current == "light" || current == SYSTEM_THEME_NAME {
        return;
    }

    let custom_themes_dir = get_custom_themes_dir();
    let watched_theme_name = current.clone();
    let watched_file_name = format!("{watched_theme_name}.json");
    let theme_file = Path::new(&custom_themes_dir).join(&watched_file_name).to_string_lossy().into_owned();

    // Only watch if the file exists
    if !Path::new(&theme_file).exists() {
        return;
    }

    let schedule_reload = {
        let watched_theme_name = watched_theme_name.clone();
        let theme_file = theme_file.clone();
        move || {
            let generation = THEME_RELOAD_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
            let watched_theme_name = watched_theme_name.clone();
            let theme_file = theme_file.clone();
            crate::utils::uncaught::spawn_thread(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if THEME_RELOAD_GENERATION.load(Ordering::SeqCst) != generation {
                    return;
                }
                // Ignore stale timers after switching themes or stopping the watcher
                if CURRENT_THEME_NAME.lock().as_deref() != Some(watched_theme_name.as_str()) {
                    return;
                }
                // Keep the last successfully loaded theme active if the file is temporarily missing
                if !Path::new(&theme_file).exists() {
                    return;
                }
                // Ignore errors (file might be in invalid state while being edited)
                if let Ok(reloaded) = load_theme_from_path(&theme_file, None) {
                    REGISTERED_THEMES.lock().insert(watched_theme_name.clone(), reloaded.clone());
                    set_global_theme(reloaded);
                    notify_theme_change();
                }
            });
        }
    };

    let listener_name = watched_theme_name.clone();
    let watcher = watch_with_error_handler(
        &custom_themes_dir,
        Arc::new(move |_event: WatchEventType, filename: Option<String>| {
            if CURRENT_THEME_NAME.lock().as_deref() != Some(listener_name.as_str()) {
                return;
            }
            match filename {
                None => schedule_reload(),
                Some(filename) if filename == watched_file_name => schedule_reload(),
                Some(_) => {}
            }
        }),
        Arc::new(|| {
            let watcher = THEME_WATCHER.lock().take();
            if let Some(watcher) = watcher {
                close_watcher(Some(&watcher));
            }
        }),
    );
    *THEME_WATCHER.lock() = watcher;
}

pub fn stop_theme_watcher() {
    THEME_RELOAD_GENERATION.fetch_add(1, Ordering::SeqCst);
    let watcher = THEME_WATCHER.lock().take();
    close_watcher(watcher.as_ref());
}

// ============================================================================
// HTML Export Helpers
// ============================================================================

fn current_theme_name_or_system(theme_name: Option<&str>) -> String {
    theme_name
        .map(str::to_string)
        .or_else(|| CURRENT_THEME_NAME.lock().clone())
        .unwrap_or_else(|| SYSTEM_THEME_NAME.to_string())
}

/// Get resolved theme colors as CSS-compatible hex strings.
/// Used by HTML export to generate CSS custom properties.
pub fn get_resolved_theme_colors(theme_name: Option<&str>) -> IndexMap<String, String> {
    let name = current_theme_name_or_system(theme_name);
    let theme = load_theme(&name, None).unwrap_or_else(|_| Arc::new(create_system_theme(None)));
    theme.colors().into_iter().map(|(token, color)| (token.as_str().to_string(), color_to_hex(&color))).collect()
}

/// Check if a theme is a "light" theme (for CSS that needs light/dark variants).
pub fn is_light_theme(theme_name: Option<&str>) -> bool {
    let name = current_theme_name_or_system(theme_name);
    let theme = load_theme(&name, None).unwrap_or_else(|_| Arc::new(create_system_theme(None)));
    theme.appearance() == TerminalTheme::Light
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetThemeExportColorsResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_bg: Option<String>,
}

/// Get explicit export colors from theme JSON, if specified.
/// Returns None for each color that isn't explicitly set.
pub fn get_theme_export_colors(theme_name: Option<&str>) -> GetThemeExportColorsResult {
    let name = current_theme_name_or_system(theme_name);
    if name == SYSTEM_THEME_NAME {
        return GetThemeExportColorsResult::default();
    }
    let Ok(theme_json) = load_theme_json(&name) else {
        return GetThemeExportColorsResult::default();
    };
    let Some(export_section) = &theme_json.export else {
        return GetThemeExportColorsResult::default();
    };
    let vars = theme_json.vars.clone().unwrap_or_default();
    // Export colors end up in CSS, which understands hex and oklch() values directly but not okhsl().
    let resolve = |value: &Option<ThemeColorValue>| -> anyhow::Result<Option<String>> {
        let Some(value) = value else { return Ok(None) };
        match resolve_var_refs(value, &vars, &mut HashSet::new())? {
            ThemeColorValue::Index(i) => {
                Ok(Some(color_to_hex(&indexed_color(i).map_err(|e| anyhow::anyhow!(e))?)))
            }
            ThemeColorValue::String(s) if s.is_empty() => Ok(None),
            ThemeColorValue::String(s) if OKHSL_FUNCTION.is_match(&s) => {
                Ok(Some(color_to_hex(&parse_color(s.as_str()).map_err(|e| anyhow::anyhow!(e))?)))
            }
            ThemeColorValue::String(s) => Ok(Some(s)),
        }
    };
    let result = (|| -> anyhow::Result<GetThemeExportColorsResult> {
        Ok(GetThemeExportColorsResult {
            page_bg: resolve(&export_section.page_bg)?,
            card_bg: resolve(&export_section.card_bg)?,
            info_bg: resolve(&export_section.info_bg)?,
        })
    })();
    result.unwrap_or_default()
}

// ============================================================================
// TUI Helpers
// ============================================================================

pub type CliHighlightTheme = IndexMap<String, Arc<dyn Fn(String) -> String + Send + Sync>>;

struct HighlightCache {
    theme: Option<Arc<Theme>>,
    highlight: Option<CliHighlightTheme>,
}

static HIGHLIGHT_CACHE: Mutex<HighlightCache> = Mutex::new(HighlightCache { theme: None, highlight: None });

fn build_cli_highlight_theme(t: &Arc<Theme>) -> CliHighlightTheme {
    let mut map: CliHighlightTheme = IndexMap::new();
    let fg = |color: ThemeColor| -> Arc<dyn Fn(String) -> String + Send + Sync> {
        let t = t.clone();
        Arc::new(move |s: String| t.fg(&color, &s))
    };
    map.insert("keyword".into(), fg(ThemeColor::SyntaxKeyword));
    map.insert("built_in".into(), fg(ThemeColor::SyntaxType));
    map.insert("literal".into(), fg(ThemeColor::SyntaxNumber));
    map.insert("number".into(), fg(ThemeColor::SyntaxNumber));
    map.insert("regexp".into(), fg(ThemeColor::SyntaxString));
    map.insert("string".into(), fg(ThemeColor::SyntaxString));
    map.insert("subst".into(), fg(ThemeColor::Text));
    map.insert("comment".into(), fg(ThemeColor::SyntaxComment));
    map.insert("doctag".into(), fg(ThemeColor::SyntaxComment));
    map.insert("meta".into(), fg(ThemeColor::Muted));
    map.insert("function".into(), fg(ThemeColor::SyntaxFunction));
    map.insert("title".into(), fg(ThemeColor::SyntaxFunction));
    map.insert("class".into(), fg(ThemeColor::SyntaxType));
    map.insert("type".into(), fg(ThemeColor::SyntaxType));
    map.insert("tag".into(), fg(ThemeColor::SyntaxPunctuation));
    map.insert("name".into(), fg(ThemeColor::SyntaxKeyword));
    map.insert("attr".into(), fg(ThemeColor::SyntaxVariable));
    map.insert("variable".into(), fg(ThemeColor::SyntaxVariable));
    map.insert("params".into(), fg(ThemeColor::SyntaxVariable));
    map.insert("operator".into(), fg(ThemeColor::SyntaxOperator));
    map.insert("punctuation".into(), fg(ThemeColor::SyntaxPunctuation));
    let tt = t.clone();
    map.insert("emphasis".into(), Arc::new(move |s: String| tt.italic(&s)));
    let tt = t.clone();
    map.insert("strong".into(), Arc::new(move |s: String| tt.bold(&s)));
    let tt = t.clone();
    map.insert("link".into(), Arc::new(move |s: String| tt.underline(&s)));
    map.insert("addition".into(), fg(ThemeColor::ToolDiffAdded));
    map.insert("deletion".into(), fg(ThemeColor::ToolDiffRemoved));
    map
}

fn get_cli_highlight_theme(t: &Arc<Theme>) -> CliHighlightTheme {
    let mut cache = HIGHLIGHT_CACHE.lock();
    let same = cache.theme.as_ref().is_some_and(|cached| Arc::ptr_eq(cached, t));
    if !same || cache.highlight.is_none() {
        cache.theme = Some(t.clone());
        cache.highlight = Some(build_cli_highlight_theme(t));
    }
    cache.highlight.clone().unwrap_or_default()
}

fn highlight_with_theme(code: &str, lang: Option<&str>, fallback_colored: bool) -> Vec<String> {
    let t = theme();
    // Validate language before highlighting to avoid stderr spam from cli-highlight
    let valid_lang = lang.filter(|l| !l.is_empty() && supports_language(l));
    // Skip highlighting when no valid language is specified: auto-detection is unreliable.
    let Some(valid_lang) = valid_lang else {
        return code.split('\n').map(|line| t.fg(&ThemeColor::MdCodeBlock, line)).collect();
    };
    let options = HighlightOptions {
        language: Some(valid_lang.to_string()),
        ignore_illegals: Some(true),
        language_subset: None,
        theme: Some(get_cli_highlight_theme(&t)),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| highlight(code, Some(&options))));
    match result {
        Ok(Ok(highlighted)) => highlighted.split('\n').map(str::to_string).collect(),
        _ => {
            if fallback_colored {
                code.split('\n').map(|line| t.fg(&ThemeColor::MdCodeBlock, line)).collect()
            } else {
                code.split('\n').map(str::to_string).collect()
            }
        }
    }
}

/// Highlight code with syntax coloring based on file extension or language.
/// Returns array of highlighted lines.
pub fn highlight_code(code: &str, lang: Option<&str>) -> Vec<String> {
    highlight_with_theme(code, lang, false)
}

/// Get language identifier from file path extension.
pub fn get_language_from_path(file_path: &str) -> Option<String> {
    let ext = file_path.rsplit('.').next()?.to_lowercase();
    if ext.is_empty() {
        return None;
    }
    let lang = match ext.as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clj" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "vim" => "vim",
        "graphql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        _ => return None,
    };
    Some(lang.to_string())
}

type StyleFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

fn themed(color: ThemeColor) -> StyleFn {
    Arc::new(move |text: &str| theme().fg(&color, text))
}

pub fn get_markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: themed(ThemeColor::MdHeading),
        link: themed(ThemeColor::MdLink),
        link_url: themed(ThemeColor::MdLinkUrl),
        code: themed(ThemeColor::MdCode),
        code_block: themed(ThemeColor::MdCodeBlock),
        code_block_border: themed(ThemeColor::MdCodeBlockBorder),
        quote: themed(ThemeColor::MdQuote),
        quote_border: themed(ThemeColor::MdQuoteBorder),
        hr: themed(ThemeColor::MdHr),
        list_bullet: themed(ThemeColor::MdListBullet),
        bold: Arc::new(|text: &str| theme().bold(text)),
        italic: Arc::new(|text: &str| theme().italic(text)),
        underline: Arc::new(|text: &str| theme().underline(text)),
        strikethrough: Arc::new(|text: &str| theme().strikethrough(text)),
        highlight_code: Some(Arc::new(|code: &str, lang: Option<&str>| highlight_with_theme(code, lang, true))),
        code_block_indent: None,
    }
}

pub fn get_select_list_theme() -> Arc<SelectListTheme> {
    Arc::new(select_list_theme())
}

fn select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: themed(ThemeColor::Accent),
        selected_text: themed(ThemeColor::Accent),
        description: themed(ThemeColor::Muted),
        scroll_info: themed(ThemeColor::Muted),
        no_match: themed(ThemeColor::Muted),
    }
}

pub fn get_editor_theme() -> Arc<EditorTheme> {
    Arc::new(EditorTheme { border_color: themed(ThemeColor::BorderMuted), select_list: select_list_theme() })
}

pub fn get_settings_list_theme() -> Arc<SettingsListTheme> {
    Arc::new(SettingsListTheme {
        label: Arc::new(|text: &str, selected: bool| {
            if selected { theme().fg(&ThemeColor::Accent, text) } else { text.to_string() }
        }),
        value: Arc::new(|text: &str, selected: bool| {
            if selected { theme().fg(&ThemeColor::Accent, text) } else { theme().fg(&ThemeColor::Muted, text) }
        }),
        description: themed(ThemeColor::Dim),
        cursor: theme().fg(&ThemeColor::Accent, "→ "),
        hint: themed(ThemeColor::Dim),
    })
}

