//! Port of `yaml` 2.9.0 `dist/parse/lexer.js` for complete input (`incomplete = false`).
//!
//! The lexer works on UTF-16 code units like the JavaScript original, so offsets and lengths
//! match the package. Tokens never split a surrogate pair (every boundary is an ASCII character).

pub(super) const BOM: &str = "\u{FEFF}";
pub(super) const DOCUMENT: &str = "\x02";
pub(super) const FLOW_END: &str = "\x18";
pub(super) const SCALAR: &str = "\x1f";

const SP: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;
const LF: u16 = b'\n' as u16;
const CR: u16 = b'\r' as u16;

fn is_empty(ch: Option<u16>) -> bool {
    matches!(ch, None | Some(SP) | Some(LF) | Some(CR) | Some(TAB))
}

/// The code unit as a `char` for matching (lone surrogates become U+FFFD).
fn cc(ch: Option<u16>) -> Option<char> {
    ch.map(|c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
}

fn is_char(ch: Option<u16>, c: char) -> bool {
    ch == Some(c as u16)
}

fn in_set(ch: Option<u16>, set: &str) -> bool {
    match ch {
        Some(c) if c < 128 => set.as_bytes().contains(&(c as u8)),
        _ => false,
    }
}

fn is_hex(ch: Option<u16>) -> bool {
    matches!(ch, Some(c) if c < 128 && (c as u8).is_ascii_hexdigit())
}

const TAG_CHARS: &str =
    "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-#;/?:@&=+$_.!~*'()";
const FLOW_INDICATOR_CHARS: &str = ",[]{}";

fn is_not_anchor_char(ch: Option<u16>) -> bool {
    ch.is_none() || in_set(ch, " ,[]{}\n\r\t")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Stream,
    BlockStart,
    Doc,
    Flow,
}

pub(super) struct Lexer<'a> {
    buf: &'a [u16],
    pos: i64,
    block_scalar_indent: i64,
    block_scalar_keep: bool,
    flow_key: bool,
    flow_level: i64,
    indent_next: i64,
    indent_value: i64,
    out: Vec<String>,
}

impl<'a> Lexer<'a> {
    /// Split `buf` into lexical tokens (the strings the JavaScript generator yields).
    pub(super) fn lex(buf: &'a [u16]) -> Vec<String> {
        let mut lexer = Lexer {
            buf,
            pos: 0,
            block_scalar_indent: -1,
            block_scalar_keep: false,
            flow_key: false,
            flow_level: 0,
            indent_next: 0,
            indent_value: 0,
            out: Vec::new(),
        };
        let mut next = State::Stream;
        while lexer.has_chars(1) {
            next = lexer.parse_next(next);
        }
        lexer.out
    }

    fn len(&self) -> i64 {
        self.buf.len() as i64
    }

    fn at(&self, i: i64) -> Option<u16> {
        if i < 0 {
            return None;
        }
        self.buf.get(i as usize).copied()
    }

    fn char_at(&self, n: i64) -> Option<u16> {
        self.at(self.pos + n)
    }

    fn has_chars(&self, n: i64) -> bool {
        self.pos + n <= self.len()
    }

    /// `buffer.substr(start, n)` (clamped).
    fn substr(&self, start: i64, n: i64) -> &'a [u16] {
        let len = self.len();
        let s = start.clamp(0, len);
        let e = (start + n.max(0)).clamp(s, len);
        &self.buf[s as usize..e as usize]
    }

    /// `buffer.slice(start, end)`.
    fn slice(&self, start: i64, end: i64) -> &'a [u16] {
        let len = self.len();
        let s = start.clamp(0, len);
        let e = end.clamp(0, len);
        if e <= s { &[] } else { &self.buf[s as usize..e as usize] }
    }

    fn index_of(&self, ch: u16, from: i64) -> i64 {
        let from = from.max(0);
        if from >= self.len() {
            return -1;
        }
        self.buf[from as usize..]
            .iter()
            .position(|c| *c == ch)
            .map(|p| p as i64 + from)
            .unwrap_or(-1)
    }

    fn emit(&mut self, s: &[u16]) {
        self.out.push(String::from_utf16_lossy(s));
    }

    fn emit_str(&mut self, s: &str) {
        self.out.push(s.to_string());
    }

    fn at_line_end(&self) -> bool {
        let mut i = self.pos;
        let mut ch = self.at(i);
        while ch == Some(SP) || ch == Some(TAB) {
            i += 1;
            ch = self.at(i);
        }
        if ch.is_none() || is_char(ch, '#') || ch == Some(LF) {
            return true;
        }
        if ch == Some(CR) {
            return self.at(i + 1) == Some(LF);
        }
        false
    }

    fn continue_scalar(&self, offset: i64) -> i64 {
        let mut ch = self.at(offset);
        if self.indent_next > 0 {
            let mut indent = 0;
            while ch == Some(SP) {
                indent += 1;
                ch = self.at(indent + offset);
            }
            if ch == Some(CR) {
                let next = self.at(indent + offset + 1);
                if next == Some(LF) {
                    return offset + indent + 1;
                }
            }
            return if ch == Some(LF) || indent >= self.indent_next {
                offset + indent
            } else {
                -1
            };
        }
        if is_char(ch, '-') || is_char(ch, '.') {
            let dt = self.substr(offset, 3);
            if (dt == utf16("---") || dt == utf16("...")) && is_empty(self.at(offset + 3)) {
                return -1;
            }
        }
        offset
    }

    /// The current line (without its line terminator) as `[start, end)` buffer indices.
    fn get_line(&self) -> (i64, i64) {
        let end = self.index_of(LF, self.pos);
        if end == -1 {
            return (self.pos, self.len());
        }
        let mut end = end;
        if self.at(end - 1) == Some(CR) {
            end -= 1;
        }
        (self.pos, end.max(self.pos))
    }

    fn parse_next(&mut self, next: State) -> State {
        match next {
            State::Stream => self.parse_stream(),
            State::BlockStart => self.parse_block_start(),
            State::Doc => self.parse_document(),
            State::Flow => self.parse_flow_collection(),
        }
    }

    fn parse_stream(&mut self) -> State {
        let (mut ls, le) = self.get_line();
        if self.at(ls) == Some(0xFEFF) {
            self.push_count(1);
            ls += 1;
        }
        let line_len = le - ls;
        let line = |i: i64| -> Option<u16> {
            if i < 0 || i >= line_len { None } else { self.buf.get((ls + i) as usize).copied() }
        };
        if is_char(line(0), '%') {
            let mut dir_end = line_len;
            let mut cs = (0..line_len).find(|i| is_char(line(*i), '#')).unwrap_or(-1);
            while cs != -1 {
                let ch = line(cs - 1);
                if ch == Some(SP) || ch == Some(TAB) {
                    dir_end = cs - 1;
                    break;
                } else {
                    cs = (cs + 1..line_len).find(|i| is_char(line(*i), '#')).unwrap_or(-1);
                }
            }
            loop {
                let ch = line(dir_end - 1);
                if ch == Some(SP) || ch == Some(TAB) {
                    dir_end -= 1;
                } else {
                    break;
                }
            }
            let n = self.push_count(dir_end) + self.push_spaces(true);
            self.push_count(line_len - n); // possible comment
            // `this.pushNewline()` without `yield*` in the original: nothing is pushed.
            return State::Stream;
        }
        if self.at_line_end() {
            let sp = self.push_spaces(true);
            self.push_count(line_len - sp);
            self.push_newline();
            return State::Stream;
        }
        self.emit_str(DOCUMENT);
        self.parse_line_start()
    }

    fn parse_line_start(&mut self) -> State {
        let ch = self.char_at(0);
        if is_char(ch, '-') || is_char(ch, '.') {
            let s = self.substr(self.pos, 3);
            if (s == utf16("---") || s == utf16("...")) && is_empty(self.char_at(3)) {
                let is_start = s == utf16("---");
                self.push_count(3);
                self.indent_value = 0;
                self.indent_next = 0;
                return if is_start { State::Doc } else { State::Stream };
            }
        }
        self.indent_value = self.push_spaces(false);
        if self.indent_next > self.indent_value && !is_empty(self.char_at(1)) {
            self.indent_next = self.indent_value;
        }
        self.parse_block_start()
    }

    fn parse_block_start(&mut self) -> State {
        let ch0 = self.char_at(0);
        let ch1 = self.char_at(1);
        if (is_char(ch0, '-') || is_char(ch0, '?') || is_char(ch0, ':')) && is_empty(ch1) {
            let n = self.push_count(1) + self.push_spaces(true);
            self.indent_next = self.indent_value + 1;
            self.indent_value += n;
            return State::BlockStart;
        }
        State::Doc
    }

    fn parse_document(&mut self) -> State {
        self.push_spaces(true);
        let (ls, le) = self.get_line();
        let line_len = le - ls;
        let mut n = self.push_indicators();
        let ch = if n < line_len { self.at(ls + n) } else { None };
        match cc(ch) {
            Some('#') | None => {
                if ch.is_some() {
                    self.push_count(line_len - n);
                }
                self.push_newline();
                self.parse_line_start()
            }
            Some('{') | Some('[') => {
                self.push_count(1);
                self.flow_key = false;
                self.flow_level = 1;
                State::Flow
            }
            Some('}') | Some(']') => {
                // this is an error
                self.push_count(1);
                State::Doc
            }
            Some('*') => {
                self.push_until(is_not_anchor_char);
                State::Doc
            }
            Some('"') | Some('\'') => self.parse_quoted_scalar(),
            Some('|') | Some('>') => {
                n += self.parse_block_scalar_header();
                n += self.push_spaces(true);
                self.push_count(line_len - n);
                self.push_newline();
                self.parse_block_scalar()
            }
            _ => self.parse_plain_scalar(),
        }
    }

    fn parse_flow_collection(&mut self) -> State {
        let mut indent = -1;
        loop {
            let nl = self.push_newline();
            let mut sp;
            if nl > 0 {
                sp = self.push_spaces(false);
                indent = sp;
                self.indent_value = sp;
            } else {
                sp = 0;
            }
            sp += self.push_spaces(true);
            if nl + sp <= 0 {
                break;
            }
        }
        let (ls, le) = self.get_line();
        let line_len = le - ls;
        let line = |i: i64| -> Option<u16> {
            if i < 0 || i >= line_len { None } else { self.buf.get((ls + i) as usize).copied() }
        };
        let starts_with = |p: &str| -> bool {
            let p = utf16(p);
            line_len >= p.len() as i64 && self.buf[ls as usize..ls as usize + p.len()] == p[..]
        };
        if (indent != -1 && indent < self.indent_next && !is_char(line(0), '#'))
            || (indent == 0 && (starts_with("---") || starts_with("...")) && is_empty(line(3)))
        {
            // Allowing for the terminal ] or } at the same (rather than greater)
            // indent level as the initial [ or { is technically invalid, but
            // failing here would be surprising to users.
            let at_flow_end_marker = indent == self.indent_next - 1
                && self.flow_level == 1
                && (is_char(line(0), ']') || is_char(line(0), '}'));
            if !at_flow_end_marker {
                // this is an error
                self.flow_level = 0;
                self.emit_str(FLOW_END);
                return self.parse_line_start();
            }
        }
        let mut n = 0;
        while is_char(line(n), ',') {
            n += self.push_count(1);
            n += self.push_spaces(true);
            self.flow_key = false;
        }
        n += self.push_indicators();
        let ch = line(n);
        match cc(ch) {
            None => State::Flow,
            Some('#') => {
                self.push_count(line_len - n);
                State::Flow
            }
            Some('{') | Some('[') => {
                self.push_count(1);
                self.flow_key = false;
                self.flow_level += 1;
                State::Flow
            }
            Some('}') | Some(']') => {
                self.push_count(1);
                self.flow_key = true;
                self.flow_level -= 1;
                if self.flow_level != 0 { State::Flow } else { State::Doc }
            }
            Some('*') => {
                self.push_until(is_not_anchor_char);
                State::Flow
            }
            Some('"') | Some('\'') => {
                self.flow_key = true;
                self.parse_quoted_scalar()
            }
            Some(':') => {
                let next = self.char_at(1);
                if self.flow_key || is_empty(next) || is_char(next, ',') {
                    self.flow_key = false;
                    self.push_count(1);
                    self.push_spaces(true);
                    return State::Flow;
                }
                self.flow_key = false;
                self.parse_plain_scalar()
            }
            _ => {
                self.flow_key = false;
                self.parse_plain_scalar()
            }
        }
    }

    fn parse_quoted_scalar(&mut self) -> State {
        let quote = self.char_at(0).unwrap_or(0);
        let mut end = self.index_of(quote, self.pos + 1);
        if quote == b'\'' as u16 {
            while end != -1 && is_char(self.at(end + 1), '\'') {
                end = self.index_of(quote, end + 2);
            }
        } else {
            // double-quote
            while end != -1 {
                let mut n = 0;
                while is_char(self.at(end - 1 - n), '\\') {
                    n += 1;
                }
                if n % 2 == 0 {
                    break;
                }
                end = self.index_of(quote, end + 1);
            }
        }
        // Only looking for newlines within the quotes
        let qb_len = end.max(0); // `substring(0, -1)` is ''
        let qb_index_of = |from: i64| -> i64 {
            let i = self.index_of(LF, from);
            if i != -1 && i < qb_len { i } else { -1 }
        };
        let mut nl = qb_index_of(self.pos);
        if nl != -1 {
            while nl != -1 {
                let cs = self.continue_scalar(nl + 1);
                if cs == -1 {
                    break;
                }
                nl = qb_index_of(cs);
            }
            if nl != -1 {
                // this is an error caused by an unexpected unindent
                end = nl - if self.at(nl - 1) == Some(CR) { 2 } else { 1 };
            }
        }
        if end == -1 {
            end = self.len();
        }
        self.push_to_index(end + 1, false);
        if self.flow_level != 0 { State::Flow } else { State::Doc }
    }

    fn parse_block_scalar_header(&mut self) -> i64 {
        self.block_scalar_indent = -1;
        self.block_scalar_keep = false;
        let mut i = self.pos;
        loop {
            i += 1;
            let ch = self.at(i);
            if is_char(ch, '+') {
                self.block_scalar_keep = true;
            } else if matches!(ch, Some(c) if c > b'0' as u16 && c <= b'9' as u16) {
                self.block_scalar_indent = (ch.unwrap() - b'0' as u16) as i64 - 1;
            } else if !is_char(ch, '-') {
                break;
            }
        }
        self.push_until(|ch| is_empty(ch) || is_char(ch, '#'))
    }

    fn parse_block_scalar(&mut self) -> State {
        let mut nl = self.pos - 1; // may be -1 if this.pos === 0
        let mut indent = 0;
        let mut i = self.pos;
        loop {
            let ch = self.at(i);
            match ch {
                Some(SP) => indent += 1,
                Some(LF) => {
                    nl = i;
                    indent = 0;
                }
                Some(CR) => {
                    if self.at(i + 1) != Some(LF) {
                        break;
                    }
                }
                _ => break,
            }
            i += 1;
        }
        if indent >= self.indent_next {
            if self.block_scalar_indent == -1 {
                self.indent_next = indent;
            } else {
                self.indent_next = self.block_scalar_indent
                    + if self.indent_next == 0 { 1 } else { self.indent_next };
            }
            loop {
                let cs = self.continue_scalar(nl + 1);
                if cs == -1 {
                    break;
                }
                nl = self.index_of(LF, cs);
                if nl == -1 {
                    break;
                }
            }
            if nl == -1 {
                nl = self.len();
            }
        }
        // Trailing insufficiently indented tabs are invalid.
        // To catch that during parsing, we include them in the block scalar value.
        let mut i = nl + 1;
        let mut ch = self.at(i);
        while ch == Some(SP) {
            i += 1;
            ch = self.at(i);
        }
        if ch == Some(TAB) {
            while matches!(ch, Some(TAB) | Some(SP) | Some(CR) | Some(LF)) {
                i += 1;
                ch = self.at(i);
            }
            nl = i - 1;
        } else if !self.block_scalar_keep {
            loop {
                let mut i = nl - 1;
                let mut ch = self.at(i);
                if ch == Some(CR) {
                    i -= 1;
                    ch = self.at(i);
                }
                let last_char = i; // Drop the line if last char not more indented
                while ch == Some(SP) {
                    i -= 1;
                    ch = self.at(i);
                }
                if ch == Some(LF) && i >= self.pos && i + 1 + indent > last_char {
                    nl = i;
                } else {
                    break;
                }
            }
        }
        self.emit_str(SCALAR);
        self.push_to_index(nl + 1, true);
        self.parse_line_start()
    }

    fn parse_plain_scalar(&mut self) -> State {
        let in_flow = self.flow_level > 0;
        let mut end = self.pos - 1;
        let mut i = self.pos - 1;
        loop {
            i += 1;
            let mut ch = self.at(i);
            if ch.is_none() {
                break;
            }
            if is_char(ch, ':') {
                let next = self.at(i + 1);
                if is_empty(next) || (in_flow && in_set(next, FLOW_INDICATOR_CHARS)) {
                    break;
                }
                end = i;
            } else if is_empty(ch) {
                let mut next = self.at(i + 1);
                if ch == Some(CR) {
                    if next == Some(LF) {
                        i += 1;
                        ch = Some(LF);
                        next = self.at(i + 1);
                    } else {
                        end = i;
                    }
                }
                if is_char(next, '#') || (in_flow && in_set(next, FLOW_INDICATOR_CHARS)) {
                    break;
                }
                if ch == Some(LF) {
                    let cs = self.continue_scalar(i + 1);
                    if cs == -1 {
                        break;
                    }
                    i = i.max(cs - 2); // to advance, but still account for ' #'
                }
            } else {
                if in_flow && in_set(ch, FLOW_INDICATOR_CHARS) {
                    break;
                }
                end = i;
            }
        }
        self.emit_str(SCALAR);
        self.push_to_index(end + 1, true);
        if in_flow { State::Flow } else { State::Doc }
    }

    fn push_count(&mut self, n: i64) -> i64 {
        if n > 0 {
            let s = self.substr(self.pos, n);
            self.emit(s);
            self.pos += n;
            return n;
        }
        0
    }

    fn push_to_index(&mut self, i: i64, allow_empty: bool) -> i64 {
        let s = self.slice(self.pos, i);
        if !s.is_empty() {
            self.emit(s);
            self.pos += s.len() as i64;
            return s.len() as i64;
        } else if allow_empty {
            self.emit_str("");
        }
        0
    }

    fn push_indicators(&mut self) -> i64 {
        let mut n = 0;
        loop {
            let ch = self.char_at(0);
            if is_char(ch, '!') {
                n += self.push_tag();
                n += self.push_spaces(true);
                continue;
            }
            if is_char(ch, '&') {
                n += self.push_until(is_not_anchor_char);
                n += self.push_spaces(true);
                continue;
            }
            if is_char(ch, '-') || is_char(ch, '?') || is_char(ch, ':') {
                let in_flow = self.flow_level > 0;
                let ch1 = self.char_at(1);
                if is_empty(ch1) || (in_flow && in_set(ch1, FLOW_INDICATOR_CHARS)) {
                    if !in_flow {
                        self.indent_next = self.indent_value + 1;
                    } else if self.flow_key {
                        self.flow_key = false;
                    }
                    n += self.push_count(1);
                    n += self.push_spaces(true);
                    continue;
                }
            }
            break;
        }
        n
    }

    fn push_tag(&mut self) -> i64 {
        if is_char(self.char_at(1), '<') {
            let mut i = self.pos + 2;
            let mut ch = self.at(i);
            while !is_empty(ch) && !is_char(ch, '>') {
                i += 1;
                ch = self.at(i);
            }
            self.push_to_index(if is_char(ch, '>') { i + 1 } else { i }, false)
        } else {
            let mut i = self.pos + 1;
            let mut ch = self.at(i);
            while ch.is_some() {
                if in_set(ch, TAG_CHARS) {
                    i += 1;
                    ch = self.at(i);
                } else if is_char(ch, '%') && is_hex(self.at(i + 1)) && is_hex(self.at(i + 2)) {
                    i += 3;
                    ch = self.at(i);
                } else {
                    break;
                }
            }
            self.push_to_index(i, false)
        }
    }

    fn push_newline(&mut self) -> i64 {
        let ch = self.at(self.pos);
        if ch == Some(LF) {
            self.push_count(1)
        } else if ch == Some(CR) && self.char_at(1) == Some(LF) {
            self.push_count(2)
        } else {
            0
        }
    }

    fn push_spaces(&mut self, allow_tabs: bool) -> i64 {
        let mut i = self.pos - 1;
        loop {
            i += 1;
            let ch = self.at(i);
            if !(ch == Some(SP) || (allow_tabs && ch == Some(TAB))) {
                break;
            }
        }
        let n = i - self.pos;
        if n > 0 {
            let s = self.substr(self.pos, n);
            self.emit(s);
            self.pos = i;
        }
        n
    }

    fn push_until(&mut self, test: impl Fn(Option<u16>) -> bool) -> i64 {
        let mut i = self.pos;
        let mut ch = self.at(i);
        while !test(ch) {
            i += 1;
            ch = self.at(i);
        }
        self.push_to_index(i, false)
    }
}

pub(super) fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}
