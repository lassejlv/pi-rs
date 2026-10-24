//! Markdown rendering. Port of `components/markdown.ts`.
//!
//! Markdown is lexed with [`crate::marked`] (GFM + CJK-friendly emphasis, strict
//! `~~` strikethrough and LaTeX math) and rendered to styled terminal lines.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::StyleFn;
use crate::latex::{RenderLatexOptions, render_latex};
use crate::marked::{self, MarkedOptions, TableCell, Token, TokenType as T};
use crate::terminal_image::{get_capabilities, hyperlink, is_image_line};
use crate::tui::Component;
use crate::utils::{apply_background_to_line, js_trim, visible_width, wrap_text_with_ansi};

/// Syntax highlighter for fenced code: `(code, lang) -> lines`.
pub type HighlightCodeFn = Arc<dyn Fn(&str, Option<&str>) -> Vec<String> + Send + Sync>;

/// Transform applied to the source before parsing: `(markdown, available_width) -> markdown`.
pub type MarkdownTransformFn = Arc<dyn Fn(&str, usize) -> String + Send + Sync>;

/// Default text styling for markdown content.
#[derive(Clone, Default)]
pub struct DefaultTextStyle {
    pub color: Option<StyleFn>,
    pub bg_color: Option<StyleFn>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

/// Theme functions for markdown elements.
#[derive(Clone)]
pub struct MarkdownTheme {
    pub heading: StyleFn,
    pub link: StyleFn,
    pub link_url: StyleFn,
    pub code: StyleFn,
    pub code_block: StyleFn,
    pub code_block_border: StyleFn,
    pub quote: StyleFn,
    pub quote_border: StyleFn,
    pub hr: StyleFn,
    pub list_bullet: StyleFn,
    pub bold: StyleFn,
    pub italic: StyleFn,
    pub strikethrough: StyleFn,
    pub underline: StyleFn,
    pub highlight_code: Option<HighlightCodeFn>,
    /// Prefix applied to each rendered code block line (default: two spaces).
    pub code_block_indent: Option<String>,
}

impl MarkdownTheme {
    /// A theme that applies no styling.
    pub fn plain() -> Self {
        let id: StyleFn = Arc::new(|t: &str| t.to_string());
        Self {
            heading: id.clone(),
            link: id.clone(),
            link_url: id.clone(),
            code: id.clone(),
            code_block: id.clone(),
            code_block_border: id.clone(),
            quote: id.clone(),
            quote_border: id.clone(),
            hr: id.clone(),
            list_bullet: id.clone(),
            bold: id.clone(),
            italic: id.clone(),
            strikethrough: id.clone(),
            underline: id,
            highlight_code: None,
            code_block_indent: None,
        }
    }
}

/// Markdown rendering options.
#[derive(Clone, Default)]
pub struct MarkdownOptions {
    /// Preserve source list markers instead of normalizing them.
    pub preserve_ordered_list_markers: bool,
    /// Preserve source backslash escapes instead of normalizing escaped punctuation.
    pub preserve_backslash_escapes: bool,
    /// Transform source Markdown before parsing, with the exact width available for content.
    pub transform: Option<MarkdownTransformFn>,
    /// Render supported LaTeX math as Unicode text (default: true).
    pub render_latex: Option<bool>,
}

enum ApplyText {
    Default,
    Identity,
    Style(StyleFn),
}

struct InlineStyleContext {
    apply_text: ApplyText,
    style_prefix: String,
}

struct State {
    text: String,
    padding_x: usize,
    padding_y: usize,
    default_text_style: Option<DefaultTextStyle>,
    theme: MarkdownTheme,
    options: MarkdownOptions,
    default_style_prefix: Option<String>,
    cache: Option<(String, usize, Vec<String>)>,
    cached_tokens: Option<(String, Arc<Vec<Token>>)>,
    /// Bumped by every change that invalidates rendered output, so a render that ran without the
    /// lock only stores its results when nothing changed meanwhile.
    generation: u64,
}

/// What a render reads from [`State`], copied out so that the transform, highlight and style
/// callbacks (which may be extension code calling back into this component) run without the lock.
struct RenderInput {
    text: String,
    padding_x: usize,
    padding_y: usize,
    default_text_style: Option<DefaultTextStyle>,
    theme: MarkdownTheme,
    options: MarkdownOptions,
    default_style_prefix: Option<String>,
    cached_tokens: Option<(String, Arc<Vec<Token>>)>,
    generation: u64,
}

/// Renders Markdown text.
pub struct Markdown {
    st: Mutex<State>,
}

/// `getStylePrefix`: the ANSI prefix a style function emits before its content.
fn style_prefix_of(style: &dyn Fn(&str) -> String) -> String {
    let styled = style("\u{0}");
    styled.find('\u{0}').map_or(String::new(), |i| styled[..i].to_string())
}

fn trim_partial_closing_fences(tokens: &mut [Token]) {
    let Some(token) = tokens.last_mut() else { return };
    match token.kind {
        T::List => {
            if let Some(item) = token.items.last_mut() {
                trim_partial_closing_fences(&mut item.tokens);
            }
        }
        T::Blockquote => trim_partial_closing_fences(&mut token.tokens),
        T::Code => {
            let marker_len = {
                let raw = token.raw.as_str();
                let first = raw.chars().next();
                match first {
                    Some(c @ ('`' | '~')) => {
                        let n = raw.chars().take_while(|&x| x == c).count();
                        if n >= 3 { Some((c, n)) } else { None }
                    }
                    _ => None,
                }
            };
            let Some((marker_char, marker_len)) = marker_len else { return };
            let last_line = token.raw.rsplit('\n').next().unwrap_or("");
            let ll = last_line.chars().count();
            if last_line.is_empty() || ll >= marker_len || last_line.chars().any(|c| c != marker_char) {
                return;
            }
            let mut text = token.text.clone();
            let cut = text.len().saturating_sub(last_line.len());
            text.truncate(cut);
            if text.ends_with('\n') {
                text.pop();
            }
            token.text = text;
        }
        _ => {}
    }
}

/// Whether marked's token object has a string `text` property.
fn has_text(kind: T) -> bool {
    matches!(
        kind,
        T::Code
            | T::Heading
            | T::Blockquote
            | T::ListItem
            | T::Html
            | T::Paragraph
            | T::Text
            | T::Escape
            | T::Link
            | T::Image
            | T::Strong
            | T::Em
            | T::Codespan
            | T::Del
            | T::Latex
            | T::LatexBlock
    )
}

fn split_js_whitespace(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
        matches!(
            c,
            '\t' | '\n'
                | '\u{0B}'
                | '\u{0C}'
                | '\r'
                | ' '
                | '\u{A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    })
    .filter(|w| !w.is_empty())
}

struct Renderer<'a> {
    theme: &'a MarkdownTheme,
    options: &'a MarkdownOptions,
    default_text_style: Option<&'a DefaultTextStyle>,
    default_style_prefix: &'a mut Option<String>,
}

impl Renderer<'_> {
    fn apply_default_style(&self, text: &str) -> String {
        let Some(style) = self.default_text_style else { return text.to_string() };
        let mut styled = text.to_string();
        if let Some(color) = &style.color {
            styled = color(&styled);
        }
        if style.bold {
            styled = (self.theme.bold)(&styled);
        }
        if style.italic {
            styled = (self.theme.italic)(&styled);
        }
        if style.strikethrough {
            styled = (self.theme.strikethrough)(&styled);
        }
        if style.underline {
            styled = (self.theme.underline)(&styled);
        }
        styled
    }

    fn get_default_style_prefix(&mut self) -> String {
        if self.default_text_style.is_none() {
            return String::new();
        }
        if let Some(p) = self.default_style_prefix.as_ref() {
            return p.clone();
        }
        let prefix = style_prefix_of(&|t| self.apply_default_style(t));
        *self.default_style_prefix = Some(prefix.clone());
        prefix
    }

    fn default_context(&mut self) -> InlineStyleContext {
        InlineStyleContext { apply_text: ApplyText::Default, style_prefix: self.get_default_style_prefix() }
    }

    fn apply(&self, ctx: &InlineStyleContext, text: &str) -> String {
        match &ctx.apply_text {
            ApplyText::Default => self.apply_default_style(text),
            ApplyText::Identity => text.to_string(),
            ApplyText::Style(f) => f(text),
        }
    }

    fn apply_with_newlines(&self, ctx: &InlineStyleContext, text: &str) -> String {
        text.split('\n').map(|s| self.apply(ctx, s)).collect::<Vec<_>>().join("\n")
    }

    fn render_latex_text(&self, token: &Token, display: bool) -> String {
        if !token.pending && self.options.render_latex != Some(false)
            && let Some(r) = render_latex(&token.text, RenderLatexOptions { display })
        {
            return r;
        }
        if display { js_trim(&token.raw).to_string() } else { token.raw.clone() }
    }

    fn render_token(
        &mut self,
        token: &Token,
        width: usize,
        next_token_type: Option<T>,
        style_context: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let spacing = |lines: &mut Vec<String>| {
            if next_token_type.is_some_and(|t| t != T::Space) {
                lines.push(String::new());
            }
        };
        match token.kind {
            T::Heading => {
                let level = token.depth;
                let prefix = format!("{} ", "#".repeat(level));
                let theme = self.theme.clone();
                let heading_style: StyleFn = if level == 1 {
                    Arc::new(move |t: &str| (theme.heading)(&(theme.bold)(&(theme.underline)(t))))
                } else {
                    Arc::new(move |t: &str| (theme.heading)(&(theme.bold)(t)))
                };
                let ctx = InlineStyleContext {
                    style_prefix: style_prefix_of(&*heading_style),
                    apply_text: ApplyText::Style(heading_style.clone()),
                };
                let heading_text = self.render_inline_tokens(&token.tokens, Some(&ctx));
                let styled = if level >= 3 { format!("{}{heading_text}", heading_style(&prefix)) } else { heading_text };
                lines.push(styled);
                spacing(&mut lines);
            }
            T::Paragraph => {
                let text = self.render_inline_tokens(&token.tokens, style_context);
                lines.push(text);
                if next_token_type.is_some_and(|t| t != T::List && t != T::Space) {
                    lines.push(String::new());
                }
            }
            T::Text => {
                lines.push(self.render_inline_tokens(std::slice::from_ref(token), style_context));
            }
            T::LatexBlock => {
                let rendered = self.render_latex_text(token, true);
                for line in rendered.split('\n') {
                    lines.push(self.apply_default_style(line));
                }
                spacing(&mut lines);
            }
            T::Code => {
                let indent = self.theme.code_block_indent.clone().unwrap_or_else(|| "  ".to_string());
                let lang = token.lang.as_deref().unwrap_or("");
                lines.push((self.theme.code_block_border)(&format!("```{lang}")));
                if let Some(highlight) = &self.theme.highlight_code {
                    let lang_arg = token.lang.as_deref().filter(|_| !token.indented);
                    for hl in highlight(&token.text, lang_arg) {
                        lines.push(format!("{indent}{hl}"));
                    }
                } else {
                    for code_line in token.text.split('\n') {
                        lines.push(format!("{indent}{}", (self.theme.code_block)(code_line)));
                    }
                }
                lines.push((self.theme.code_block_border)("```"));
                spacing(&mut lines);
            }
            T::List => {
                lines.extend(self.render_list(token, 0, width, style_context));
            }
            T::Table => {
                lines.extend(self.render_table(token, width, next_token_type, style_context));
            }
            T::Blockquote => {
                // A border needs two columns plus at least one content column. Once that
                // no longer fits, pass children through without rewrapping or adding borders;
                // otherwise each extra nesting level multiplies the number of output lines.
                if width < 3 {
                    // Traverse borderless quotes iteratively so extra nesting does not
                    // consume renderer stack frames either.
                    let mut pending = vec![(token, next_token_type)];
                    while let Some((child, next)) = pending.pop() {
                        if child.kind == T::Blockquote {
                            for (i, qt) in child.tokens.iter().enumerate().rev() {
                                pending.push((qt, child.tokens.get(i + 1).map(|t| t.kind)));
                            }
                        } else {
                            lines.extend(self.render_token(child, width, next, style_context));
                        }
                    }
                    return lines;
                }
                let theme = self.theme.clone();
                let quote_style: StyleFn = Arc::new(move |t: &str| (theme.quote)(&(theme.italic)(t)));
                let quote_prefix = style_prefix_of(&*quote_style);
                let apply_quote_style = |line: &str| -> String {
                    if quote_prefix.is_empty() {
                        return quote_style(line);
                    }
                    quote_style(&line.replace("\x1b[0m", &format!("\x1b[0m{quote_prefix}")))
                };
                let quote_content_width = width - 2;
                let ctx = InlineStyleContext { apply_text: ApplyText::Identity, style_prefix: quote_prefix.clone() };
                let mut rendered: Vec<String> = Vec::new();
                for (i, qt) in token.tokens.iter().enumerate() {
                    let next = token.tokens.get(i + 1).map(|t| t.kind);
                    rendered.extend(self.render_token(qt, quote_content_width, next, Some(&ctx)));
                }
                while rendered.last().is_some_and(|l| l.is_empty()) {
                    rendered.pop();
                }
                for quote_line in &rendered {
                    let styled = apply_quote_style(quote_line);
                    for wrapped in wrap_text_with_ansi(&styled, quote_content_width) {
                        lines.push(format!("{}{wrapped}", (self.theme.quote_border)("│ ")));
                    }
                }
                spacing(&mut lines);
            }
            T::Hr => {
                lines.push((self.theme.hr)(&"─".repeat(width.min(80))));
                spacing(&mut lines);
            }
            T::Html => {
                lines.push(self.apply_default_style(js_trim(&token.raw)));
            }
            T::Space => lines.push(String::new()),
            kind => {
                if has_text(kind) {
                    lines.push(token.text.clone());
                }
            }
        }
        lines
    }

    fn render_inline_tokens(&mut self, tokens: &[Token], style_context: Option<&InlineStyleContext>) -> String {
        let owned;
        let ctx = match style_context {
            Some(c) => c,
            None => {
                owned = self.default_context();
                &owned
            }
        };
        let prefix = ctx.style_prefix.clone();
        let mut result = String::new();
        for token in tokens {
            match token.kind {
                T::Latex => {
                    let rendered = self.render_latex_text(token, false);
                    result.push_str(&self.apply_with_newlines(ctx, &rendered));
                }
                T::Escape => {
                    let text = if self.options.preserve_backslash_escapes { &token.raw } else { &token.text };
                    result.push_str(&self.apply_with_newlines(ctx, text));
                }
                T::Text => {
                    if !token.tokens.is_empty() {
                        result.push_str(&self.render_inline_tokens(&token.tokens, Some(ctx)));
                    } else {
                        result.push_str(&self.apply_with_newlines(ctx, &token.text));
                    }
                }
                T::Paragraph => result.push_str(&self.render_inline_tokens(&token.tokens, Some(ctx))),
                T::Strong => {
                    let content = self.render_inline_tokens(&token.tokens, Some(ctx));
                    result.push_str(&(self.theme.bold)(&content));
                    result.push_str(&prefix);
                }
                T::Em => {
                    let content = self.render_inline_tokens(&token.tokens, Some(ctx));
                    result.push_str(&(self.theme.italic)(&content));
                    result.push_str(&prefix);
                }
                T::Codespan => {
                    result.push_str(&(self.theme.code)(&token.text));
                    result.push_str(&prefix);
                }
                T::Link => {
                    let link_text = self.render_inline_tokens(&token.tokens, Some(ctx));
                    let styled = (self.theme.link)(&(self.theme.underline)(&link_text));
                    if get_capabilities().hyperlinks {
                        result.push_str(&hyperlink(&styled, &token.href));
                        result.push_str(&prefix);
                    } else {
                        let href_cmp = token.href.strip_prefix("mailto:").unwrap_or(&token.href);
                        if token.text == token.href || token.text == href_cmp {
                            result.push_str(&styled);
                        } else {
                            result.push_str(&styled);
                            result.push_str(&(self.theme.link_url)(&format!(" ({})", token.href)));
                        }
                        result.push_str(&prefix);
                    }
                }
                T::Br => result.push('\n'),
                T::Del => {
                    let content = self.render_inline_tokens(&token.tokens, Some(ctx));
                    result.push_str(&(self.theme.strikethrough)(&content));
                    result.push_str(&prefix);
                }
                T::Html => result.push_str(&self.apply_with_newlines(ctx, &token.raw)),
                kind => {
                    if has_text(kind) {
                        result.push_str(&self.apply_with_newlines(ctx, &token.text));
                    }
                }
            }
        }
        if !prefix.is_empty() {
            while result.ends_with(&prefix) {
                result.truncate(result.len() - prefix.len());
            }
        }
        result
    }

    fn ordered_list_marker(item: &Token) -> Option<String> {
        static RE: std::sync::LazyLock<marked::js_regex::JsRegex> =
            std::sync::LazyLock::new(|| marked::js_regex::JsRegex::new(r"^(?: {0,3})(\d{1,9}[.)])[ \t]+", ""));
        RE.exec(&item.raw).map(|m| format!("{} ", marked::js_regex::group(&m, 1)))
    }

    fn unordered_list_marker(item: &Token) -> Option<String> {
        static RE: std::sync::LazyLock<marked::js_regex::JsRegex> =
            std::sync::LazyLock::new(|| marked::js_regex::JsRegex::new(r"^(?: {0,3})([-+*])(?:[ \t]+|(?=\r?\n|$))", ""));
        RE.exec(&item.raw).map(|m| format!("{} ", marked::js_regex::group(&m, 1)))
    }

    fn render_list(
        &mut self,
        token: &Token,
        depth: usize,
        width: usize,
        style_context: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let indent = "    ".repeat(depth);
        let start_number = token.start.unwrap_or(1);
        let n = token.items.len();
        for (i, item) in token.items.iter().enumerate() {
            let is_last = i == n - 1;
            let bullet = if token.ordered {
                if self.options.preserve_ordered_list_markers {
                    Self::ordered_list_marker(item).unwrap_or_else(|| format!("{}. ", start_number + i as u64))
                } else {
                    format!("{}. ", start_number + i as u64)
                }
            } else if self.options.preserve_ordered_list_markers {
                Self::unordered_list_marker(item).unwrap_or_else(|| "- ".to_string())
            } else {
                "- ".to_string()
            };
            let task_marker = if item.task {
                format!("[{}] ", if item.checked == Some(true) { "x" } else { " " })
            } else {
                String::new()
            };
            let marker = format!("{bullet}{task_marker}");
            let first_prefix = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let continuation_prefix = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            let item_width = width.saturating_sub(visible_width(&first_prefix)).max(1);
            let mut rendered_any = false;
            for item_token in &item.tokens {
                if item_token.kind == T::List {
                    lines.extend(self.render_list(item_token, depth + 1, width, style_context));
                    rendered_any = true;
                    continue;
                }
                let item_lines = self.render_token(item_token, item_width, None, style_context);
                for line in &item_lines {
                    for wrapped in wrap_text_with_ansi(line, item_width) {
                        let p = if rendered_any { &continuation_prefix } else { &first_prefix };
                        lines.push(format!("{p}{wrapped}"));
                        rendered_any = true;
                    }
                }
            }
            if !rendered_any {
                lines.push(first_prefix.clone());
            }
            if token.loose && !is_last {
                lines.push(String::new());
            }
        }
        lines
    }

    fn longest_word_width(text: &str, max_width: Option<usize>) -> usize {
        let longest = split_js_whitespace(text).map(visible_width).max().unwrap_or(0);
        match max_width {
            None => longest,
            Some(m) => longest.min(m),
        }
    }

    fn wrap_cell_text(text: &str, max_width: usize, style_prefix: &str) -> Vec<String> {
        let lines = wrap_text_with_ansi(text, max_width.max(1));
        let n = lines.len();
        lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let reset = if i < n - 1 { "\x1b[22;23;24;25;27;28;29;39m" } else { "" };
                format!("{line}{reset}{style_prefix}")
            })
            .collect()
    }

    fn cell_text(&mut self, cell: &TableCell, style_context: Option<&InlineStyleContext>) -> String {
        self.render_inline_tokens(&cell.tokens, style_context)
    }

    fn render_table(
        &mut self,
        token: &Token,
        available_width: usize,
        next_token_type: Option<T>,
        style_context: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let num_cols = token.header.len();
        if num_cols == 0 {
            return lines;
        }
        let border_overhead = 3 * num_cols + 1;
        let available_for_cells = available_width as isize - border_overhead as isize;
        if available_for_cells < num_cols as isize {
            let mut fallback = if token.raw.is_empty() { Vec::new() } else { wrap_text_with_ansi(&token.raw, available_width) };
            if next_token_type.is_some_and(|t| t != T::Space) {
                fallback.push(String::new());
            }
            return fallback;
        }
        let available_for_cells = available_for_cells as usize;
        let max_unbroken_word_width = 30;

        let mut natural_widths = vec![0usize; num_cols];
        let mut min_word_widths = vec![1usize; num_cols];
        for i in 0..num_cols {
            let header_text = self.cell_text(&token.header[i], style_context);
            natural_widths[i] = visible_width(&header_text);
            min_word_widths[i] = Self::longest_word_width(&header_text, Some(max_unbroken_word_width)).max(1);
        }
        for row in &token.rows {
            for (i, cell) in row.iter().enumerate() {
                let cell_text = self.cell_text(cell, style_context);
                if i >= natural_widths.len() {
                    natural_widths.push(0);
                    min_word_widths.push(1);
                }
                natural_widths[i] = natural_widths[i].max(visible_width(&cell_text));
                min_word_widths[i] =
                    min_word_widths[i].max(Self::longest_word_width(&cell_text, Some(max_unbroken_word_width)));
            }
        }
        natural_widths.truncate(num_cols);
        min_word_widths.truncate(num_cols);

        let mut min_column_widths = min_word_widths.clone();
        let mut min_cells_width: usize = min_column_widths.iter().sum();
        if min_cells_width > available_for_cells {
            min_column_widths = vec![1; num_cols];
            let remaining = available_for_cells as isize - num_cols as isize;
            if remaining > 0 {
                let remaining = remaining as usize;
                let total_weight: usize = min_word_widths.iter().map(|w| w.saturating_sub(1)).sum();
                let growth: Vec<usize> = min_word_widths
                    .iter()
                    .map(|w| {
                        let weight = w.saturating_sub(1);
                        if total_weight > 0 { (weight as f64 / total_weight as f64 * remaining as f64).floor() as usize } else { 0 }
                    })
                    .collect();
                for i in 0..num_cols {
                    min_column_widths[i] += growth[i];
                }
                let allocated: usize = growth.iter().sum();
                let mut leftover = remaining.saturating_sub(allocated);
                let mut i = 0;
                while leftover > 0 && i < num_cols {
                    min_column_widths[i] += 1;
                    leftover -= 1;
                    i += 1;
                }
            }
            min_cells_width = min_column_widths.iter().sum();
        }

        let total_natural_width: usize = natural_widths.iter().sum::<usize>() + border_overhead;
        let column_widths: Vec<usize> = if total_natural_width <= available_width {
            natural_widths.iter().zip(&min_column_widths).map(|(n, m)| (*n).max(*m)).collect()
        } else {
            let total_grow_potential: usize =
                natural_widths.iter().zip(&min_column_widths).map(|(n, m)| n.saturating_sub(*m)).sum();
            let extra_width = available_for_cells.saturating_sub(min_cells_width);
            let mut widths: Vec<usize> = min_column_widths
                .iter()
                .enumerate()
                .map(|(index, &min_width)| {
                    let delta = natural_widths[index].saturating_sub(min_width);
                    let grow = if total_grow_potential > 0 {
                        (delta as f64 / total_grow_potential as f64 * extra_width as f64).floor() as usize
                    } else {
                        0
                    };
                    min_width + grow
                })
                .collect();
            let allocated: usize = widths.iter().sum();
            let mut remaining = available_for_cells as isize - allocated as isize;
            while remaining > 0 {
                let mut grew = false;
                let mut i = 0;
                while i < num_cols && remaining > 0 {
                    if widths[i] < natural_widths[i] {
                        widths[i] += 1;
                        remaining -= 1;
                        grew = true;
                    }
                    i += 1;
                }
                if !grew {
                    break;
                }
            }
            widths
        };

        let border = |l: &str, m: &str, r: &str| {
            let cells: Vec<String> = column_widths.iter().map(|w| "─".repeat(*w)).collect();
            format!("{l}─{}─{r}", cells.join(&format!("─{m}─")))
        };
        lines.push(border("┌", "┬", "┐"));

        let style_prefix = style_context.map_or(String::new(), |c| c.style_prefix.clone());
        let header_cells: Vec<Vec<String>> = token
            .header
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let text = self.cell_text(cell, style_context);
                Self::wrap_cell_text(&text, column_widths[i], &style_prefix)
            })
            .collect();
        let header_line_count = header_cells.iter().map(Vec::len).max().unwrap_or(0);
        for line_idx in 0..header_line_count {
            let parts: Vec<String> = header_cells
                .iter()
                .enumerate()
                .map(|(col, cell_lines)| {
                    let text = cell_lines.get(line_idx).map_or("", |s| s.as_str());
                    let padded = format!("{text}{}", " ".repeat(column_widths[col].saturating_sub(visible_width(text))));
                    (self.theme.bold)(&padded)
                })
                .collect();
            lines.push(format!("│ {} │", parts.join(" │ ")));
        }

        let separator = border("├", "┼", "┤");
        lines.push(separator.clone());

        let row_count = token.rows.len();
        for (row_index, row) in token.rows.iter().enumerate() {
            let row_cells: Vec<Vec<String>> = row
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    let text = self.cell_text(cell, style_context);
                    Self::wrap_cell_text(&text, column_widths.get(i).copied().unwrap_or(0), &style_prefix)
                })
                .collect();
            let row_line_count = row_cells.iter().map(Vec::len).max().unwrap_or(0);
            for line_idx in 0..row_line_count {
                let parts: Vec<String> = row_cells
                    .iter()
                    .enumerate()
                    .map(|(col, cell_lines)| {
                        let text = cell_lines.get(line_idx).map_or("", |s| s.as_str());
                        let w = column_widths.get(col).copied().unwrap_or(0);
                        format!("{text}{}", " ".repeat(w.saturating_sub(visible_width(text))))
                    })
                    .collect();
                lines.push(format!("│ {} │", parts.join(" │ ")));
            }
            if row_index < row_count - 1 {
                lines.push(separator.clone());
            }
        }

        lines.push(border("└", "┴", "┘"));
        if next_token_type.is_some_and(|t| t != T::Space) {
            lines.push(String::new());
        }
        lines
    }
}

impl Markdown {
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_text_style: Option<DefaultTextStyle>,
        options: Option<MarkdownOptions>,
    ) -> Self {
        Self {
            st: Mutex::new(State {
                text: text.into(),
                padding_x,
                padding_y,
                default_text_style,
                theme,
                options: options.unwrap_or_default(),
                default_style_prefix: None,
                cache: None,
                cached_tokens: None,
                generation: 0,
            }),
        }
    }

    /// `Markdown::new` wrapped in an `Arc`.
    pub fn new_ref(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_text_style: Option<DefaultTextStyle>,
        options: Option<MarkdownOptions>,
    ) -> Arc<Self> {
        Arc::new(Self::new(text, padding_x, padding_y, theme, default_text_style, options))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn text(&self) -> String {
        self.lock().text.clone()
    }

    pub fn set_text(&self, text: impl Into<String>) {
        let mut s = self.lock();
        s.text = text.into();
        s.cache = None;
        s.generation += 1;
    }

    pub fn set_theme(&self, theme: MarkdownTheme) {
        let mut s = self.lock();
        s.theme = theme;
        s.default_style_prefix = None;
        s.cache = None;
        s.generation += 1;
    }

    pub fn set_default_text_style(&self, style: Option<DefaultTextStyle>) {
        let mut s = self.lock();
        s.default_text_style = style;
        s.default_style_prefix = None;
        s.cache = None;
        s.generation += 1;
    }
}

impl Markdown {
    /// Render `input` at `width`. Runs without the state lock; returns the lines to cache (empty
    /// for blank text, which renders nothing) plus the tokens and default style prefix to cache.
    fn render_unlocked(
        input: RenderInput,
        width: usize,
    ) -> (Vec<String>, Option<(String, Arc<Vec<Token>>)>, Option<String>, bool) {
        let RenderInput {
            text: source,
            padding_x,
            padding_y,
            default_text_style,
            theme,
            options,
            mut default_style_prefix,
            cached_tokens,
            ..
        } = input;
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let text = match &options.transform {
            Some(t) => t(&source, content_width),
            None => source,
        };
        if text.is_empty() || js_trim(&text).is_empty() {
            return (Vec::new(), cached_tokens, default_style_prefix, true);
        }

        let normalized = text.replace('\t', "   ");
        let (tokens, cached_tokens) = match cached_tokens {
            Some((src, tokens)) if src == normalized => (tokens.clone(), Some((src, tokens))),
            _ => {
                let mut tokens = marked::lexer(&normalized, MarkedOptions::pi());
                trim_partial_closing_fences(&mut tokens);
                let tokens = Arc::new(tokens);
                (tokens.clone(), Some((normalized, tokens)))
            }
        };

        let mut renderer = Renderer {
            theme: &theme,
            options: &options,
            default_text_style: default_text_style.as_ref(),
            default_style_prefix: &mut default_style_prefix,
        };
        let mut rendered_lines: Vec<String> = Vec::new();
        for (i, token) in tokens.iter().enumerate() {
            let next = tokens.get(i + 1).map(|t| t.kind);
            rendered_lines.extend(renderer.render_token(token, content_width, next, None));
        }

        let mut wrapped_lines: Vec<String> = Vec::new();
        for line in &rendered_lines {
            if is_image_line(line) {
                wrapped_lines.push(line.clone());
            } else {
                wrapped_lines.extend(wrap_text_with_ansi(line, content_width));
            }
        }

        let margin = " ".repeat(padding_x);
        let bg_fn = default_text_style.as_ref().and_then(|d| d.bg_color.clone());
        let mut content_lines: Vec<String> = Vec::new();
        for line in &wrapped_lines {
            if is_image_line(line) {
                content_lines.push(line.clone());
                continue;
            }
            let with_margins = format!("{margin}{line}{margin}");
            if let Some(bg) = &bg_fn {
                content_lines.push(apply_background_to_line(&with_margins, width, &**bg));
            } else {
                let pad = width.saturating_sub(visible_width(&with_margins));
                content_lines.push(format!("{with_margins}{}", " ".repeat(pad)));
            }
        }

        let empty_line = " ".repeat(width);
        let empty_lines: Vec<String> = (0..padding_y)
            .map(|_| match &bg_fn {
                Some(bg) => apply_background_to_line(&empty_line, width, &**bg),
                None => empty_line.clone(),
            })
            .collect();
        let mut result = empty_lines.clone();
        result.extend(content_lines);
        result.extend(empty_lines);
        (result, cached_tokens, default_style_prefix, false)
    }
}

impl Component for Markdown {
    fn render(&self, width: usize) -> Vec<String> {
        let input = {
            let s = self.lock();
            if let Some((text, w, lines)) = &s.cache
                && *text == s.text
                && *w == width
            {
                return lines.clone();
            }
            RenderInput {
                text: s.text.clone(),
                padding_x: s.padding_x,
                padding_y: s.padding_y,
                default_text_style: s.default_text_style.clone(),
                theme: s.theme.clone(),
                options: s.options.clone(),
                default_style_prefix: s.default_style_prefix.clone(),
                cached_tokens: s.cached_tokens.clone(),
                generation: s.generation,
            }
        };
        let (text, generation) = (input.text.clone(), input.generation);
        let (result, cached_tokens, default_style_prefix, blank) = Self::render_unlocked(input, width);

        let mut s = self.lock();
        if s.generation == generation {
            s.cache = Some((text, width, result.clone()));
            s.cached_tokens = cached_tokens;
            s.default_style_prefix = default_style_prefix;
        }
        drop(s);
        if result.is_empty() && !blank { vec![String::new()] } else { result }
    }

    fn invalidate(&self) {
        let mut s = self.lock();
        s.cache = None;
        s.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{OnceLock, Weak};

    use super::*;

    #[test]
    fn deeply_nested_blockquote_tokens_render_without_output_growth() {
        // Exercise the renderer directly: the recursive lexer can exhaust the small
        // test-thread stack before reaching the renderer at this depth.
        let mut token = Token::new(T::Text, "x");
        token.text = "x".to_string();
        for _ in 0..200 {
            let mut quote = Token::new(T::Blockquote, "");
            quote.tokens.push(token);
            token = quote;
        }
        let theme = MarkdownTheme::plain();
        let options = MarkdownOptions::default();
        let mut prefix = None;
        let mut renderer = Renderer {
            theme: &theme,
            options: &options,
            default_text_style: None,
            default_style_prefix: &mut prefix,
        };
        for width in [1, 2, 3, 4, 80] {
            assert_eq!(renderer.render_token(&token, width, None, None), [format!("{}x", "│ ".repeat((width - 1) / 2))]);
        }
    }

    /// Callbacks (extension code) may call back into the component they render for.
    #[test]
    fn transform_highlight_and_style_callbacks_run_without_the_component_lock() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let this: Arc<OnceLock<Weak<Markdown>>> = Arc::default();
            let component = |this: &Arc<OnceLock<Weak<Markdown>>>| this.get().and_then(Weak::upgrade).unwrap();
            let mut theme = MarkdownTheme::plain();
            let in_highlight = this.clone();
            theme.highlight_code = Some(Arc::new(move |code: &str, _lang: Option<&str>| {
                vec![format!("{}|{code}", component(&in_highlight).text().len())]
            }));
            let in_style = this.clone();
            theme.bold = Arc::new(move |text: &str| {
                component(&in_style).invalidate();
                format!("*{text}*")
            });
            let in_transform = this.clone();
            let options = MarkdownOptions {
                transform: Some(Arc::new(move |text: &str, _width: usize| {
                    // Reading and even replacing the text from inside a render must not deadlock.
                    let markdown = component(&in_transform);
                    assert_eq!(markdown.text(), text);
                    text.to_string()
                })),
                ..Default::default()
            };
            let markdown = Markdown::new_ref("**hi**\n\n```\nx\n```", 0, 0, theme, None, Some(options));
            this.set(Arc::downgrade(&markdown)).unwrap();
            let lines: Vec<String> = markdown.render(20).iter().map(|l| l.trim_end().to_string()).collect();
            assert_eq!(lines, ["*hi*", "", "```", "  17|x", "```"]);
            // The bold callback invalidated the component during the render, so the result was not
            // cached: the next render runs the callbacks again.
            let in_set = Arc::downgrade(&markdown);
            let markdown2 = Markdown::new_ref(
                "a",
                0,
                0,
                MarkdownTheme::plain(),
                None,
                Some(MarkdownOptions {
                    transform: Some(Arc::new(move |text: &str, _| {
                        if let Some(markdown) = in_set.upgrade() {
                            markdown.set_text("changed");
                        }
                        text.to_string()
                    })),
                    ..Default::default()
                }),
            );
            assert_eq!(markdown2.render(5)[0].trim_end(), "a");
            assert_eq!(markdown.text(), "changed");
            done_tx.send(()).unwrap();
        });
        done_rx.recv_timeout(std::time::Duration::from_secs(10)).expect("render deadlocked");
    }

    #[test]
    fn a_render_that_raced_a_change_is_not_cached() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let this: Arc<OnceLock<Weak<Markdown>>> = Arc::default();
        let (counter, weak) = (calls.clone(), this.clone());
        let options = MarkdownOptions {
            transform: Some(Arc::new(move |text: &str, _| {
                if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    weak.get().and_then(Weak::upgrade).unwrap().set_text("second");
                }
                text.to_string()
            })),
            ..Default::default()
        };
        let markdown = Markdown::new_ref("first", 0, 0, MarkdownTheme::plain(), None, Some(options));
        this.set(Arc::downgrade(&markdown)).unwrap();
        assert_eq!(markdown.render(10)[0].trim_end(), "first");
        assert_eq!(markdown.render(10)[0].trim_end(), "second");
        assert_eq!(markdown.render(10)[0].trim_end(), "second");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
