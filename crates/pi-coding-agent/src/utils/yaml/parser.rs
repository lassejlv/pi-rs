//! Port of `yaml` 2.9.0 `dist/parse/parser.js` and `dist/parse/cst.js` (`tokenType`).
//!
//! The CST tokens mirror the JavaScript objects; `type` strings are kept so that messages that
//! interpolate them match.

use super::lexer::{BOM, DOCUMENT, FLOW_END, Lexer, SCALAR};

pub(super) fn ulen(s: &str) -> i64 {
    s.encode_utf16().count() as i64
}

/// A lexical token, or a flow scalar (`alias`, `scalar`, `single-quoted-scalar`,
/// `double-quoted-scalar`) which may carry an `end`.
#[derive(Debug, Clone)]
pub(super) struct St {
    pub ty: &'static str,
    pub offset: i64,
    pub indent: i64,
    pub source: String,
    pub end: Option<Vec<Token>>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Item {
    pub start: Vec<Token>,
    pub key: Option<Box<Token>>,
    pub sep: Option<Vec<Token>>,
    pub value: Option<Box<Token>>,
    pub explicit_key: bool,
}

#[derive(Debug, Clone)]
pub(super) enum Token {
    Src(St),
    BlockScalar {
        offset: i64,
        indent: i64,
        props: Vec<Token>,
        source: String,
    },
    BlockMap {
        offset: i64,
        indent: i64,
        items: Vec<Item>,
    },
    BlockSeq {
        offset: i64,
        indent: i64,
        items: Vec<Item>,
    },
    Flow {
        offset: i64,
        indent: i64,
        start: St,
        items: Vec<Item>,
        end: Vec<Token>,
    },
    Document {
        offset: i64,
        start: Vec<Token>,
        value: Option<Box<Token>>,
        end: Option<Vec<Token>>,
    },
    DocEnd {
        offset: i64,
        source: String,
        end: Option<Vec<Token>>,
    },
    Directive {
        offset: i64,
        source: String,
    },
    Error {
        offset: i64,
        source: String,
        message: String,
    },
}

impl Token {
    pub fn ty(&self) -> &'static str {
        match self {
            Token::Src(st) => st.ty,
            Token::BlockScalar { .. } => "block-scalar",
            Token::BlockMap { .. } => "block-map",
            Token::BlockSeq { .. } => "block-seq",
            Token::Flow { .. } => "flow-collection",
            Token::Document { .. } => "document",
            Token::DocEnd { .. } => "doc-end",
            Token::Directive { .. } => "directive",
            Token::Error { .. } => "error",
        }
    }

    pub fn offset(&self) -> i64 {
        match self {
            Token::Src(st) => st.offset,
            Token::BlockScalar { offset, .. }
            | Token::BlockMap { offset, .. }
            | Token::BlockSeq { offset, .. }
            | Token::Flow { offset, .. }
            | Token::Document { offset, .. }
            | Token::DocEnd { offset, .. }
            | Token::Directive { offset, .. }
            | Token::Error { offset, .. } => *offset,
        }
    }

    /// `token.source` when it is a string.
    pub fn source(&self) -> Option<&str> {
        match self {
            Token::Src(st) => Some(&st.source),
            Token::BlockScalar { source, .. }
            | Token::DocEnd { source, .. }
            | Token::Directive { source, .. }
            | Token::Error { source, .. } => Some(source),
            _ => None,
        }
    }

    /// `'indent' in token ? token.indent : undefined`.
    pub fn indent(&self) -> Option<i64> {
        match self {
            Token::Src(st) => Some(st.indent),
            Token::BlockScalar { indent, .. }
            | Token::BlockMap { indent, .. }
            | Token::BlockSeq { indent, .. }
            | Token::Flow { indent, .. } => Some(*indent),
            _ => None,
        }
    }

    fn set_indent(&mut self, value: i64) {
        match self {
            Token::Src(st) => st.indent = value,
            Token::BlockScalar { indent, .. }
            | Token::BlockMap { indent, .. }
            | Token::BlockSeq { indent, .. }
            | Token::Flow { indent, .. } => *indent = value,
            _ => {}
        }
    }

    /// `token.end` when it is an array.
    pub fn end_array(&self) -> Option<&Vec<Token>> {
        match self {
            Token::Src(st) => st.end.as_ref(),
            Token::Flow { end, .. } => Some(end),
            Token::Document { end, .. } | Token::DocEnd { end, .. } => end.as_ref(),
            _ => None,
        }
    }

    fn end_array_mut(&mut self) -> Option<&mut Vec<Token>> {
        match self {
            Token::Src(st) => st.end.as_mut(),
            Token::Flow { end, .. } => Some(end),
            Token::Document { end, .. } | Token::DocEnd { end, .. } => end.as_mut(),
            _ => None,
        }
    }

    /// `if (token.end) token.end.push(st) else token.end = [st]`.
    fn push_end(&mut self, st: Token) {
        match self {
            Token::Src(s) => s.end.get_or_insert_with(Vec::new).push(st),
            Token::Flow { end, .. } => end.push(st),
            Token::Document { end, .. } | Token::DocEnd { end, .. } => {
                end.get_or_insert_with(Vec::new).push(st)
            }
            _ => {}
        }
    }

}

/// `tokenType(source)`.
fn token_type(source: &str) -> Option<&'static str> {
    match source {
        BOM => return Some("byte-order-mark"),
        DOCUMENT => return Some("doc-mode"),
        FLOW_END => return Some("flow-error-end"),
        SCALAR => return Some("scalar"),
        "---" => return Some("doc-start"),
        "..." => return Some("doc-end"),
        "" | "\n" | "\r\n" => return Some("newline"),
        "-" => return Some("seq-item-ind"),
        "?" => return Some("explicit-key-ind"),
        ":" => return Some("map-value-ind"),
        "{" => return Some("flow-map-start"),
        "}" => return Some("flow-map-end"),
        "[" => return Some("flow-seq-start"),
        "]" => return Some("flow-seq-end"),
        "," => return Some("comma"),
        _ => {}
    }
    match source.chars().next() {
        Some(' ') | Some('\t') => Some("space"),
        Some('#') => Some("comment"),
        Some('%') => Some("directive-line"),
        Some('*') => Some("alias"),
        Some('&') => Some("anchor"),
        Some('!') => Some("tag"),
        Some('\'') => Some("single-quoted-scalar"),
        Some('"') => Some("double-quoted-scalar"),
        Some('|') | Some('>') => Some("block-scalar-header"),
        _ => None,
    }
}

fn includes_token(list: &[Token], ty: &str) -> bool {
    list.iter().any(|t| t.ty() == ty)
}

fn find_non_empty_index(list: &[Token]) -> i64 {
    for (i, t) in list.iter().enumerate() {
        match t.ty() {
            "space" | "comment" | "newline" => {}
            _ => return i as i64,
        }
    }
    -1
}

fn is_flow_token(token: Option<&Token>) -> bool {
    matches!(
        token.map(|t| t.ty()),
        Some("alias" | "scalar" | "single-quoted-scalar" | "double-quoted-scalar" | "flow-collection")
    )
}

/// `getPrevProps(parent)`: the props array of the parent's last item (or the document start).
fn get_prev_props(parent: &mut Token) -> Option<&mut Vec<Token>> {
    match parent {
        Token::Document { start, .. } => Some(start),
        Token::BlockMap { items, .. } => {
            let it = items.last_mut()?;
            match it.sep {
                Some(ref mut sep) => Some(sep),
                None => Some(&mut it.start),
            }
        }
        Token::BlockSeq { items, .. } => items.last_mut().map(|it| &mut it.start),
        _ => None,
    }
}

/// `getFirstKeyStartProps(prev)`: splices the trailing key props off `prev`.
fn get_first_key_start_props(prev: Option<&mut Vec<Token>>) -> Vec<Token> {
    let Some(prev) = prev else { return Vec::new() };
    if prev.is_empty() {
        return Vec::new();
    }
    let mut i = prev.len() as i64;
    loop {
        i -= 1;
        if i < 0 {
            break;
        }
        match prev[i as usize].ty() {
            "doc-start" | "explicit-key-ind" | "map-value-ind" | "seq-item-ind" | "newline" => break,
            _ => {}
        }
    }
    loop {
        i += 1;
        if (i as usize) < prev.len() && prev[i as usize].ty() == "space" {
            continue;
        }
        break;
    }
    let i = (i.max(0) as usize).min(prev.len());
    prev.split_off(i)
}

fn fix_flow_seq_items(fc: &mut Token) {
    let Token::Flow { start, items, .. } = fc else { return };
    if start.ty != "flow-seq-start" {
        return;
    }
    for it in items.iter_mut() {
        let Some(sep) = it.sep.as_ref() else { continue };
        if it.value.is_none()
            && !includes_token(&it.start, "explicit-key-ind")
            && !includes_token(sep, "map-value-ind")
        {
            if let Some(key) = it.key.take() {
                it.value = Some(key);
            }
            let sep = it.sep.take().unwrap_or_default();
            if is_flow_token(it.value.as_deref()) {
                let value = it.value.as_mut().unwrap();
                match value.as_mut() {
                    Token::Src(st) => match st.end {
                        Some(ref mut end) => end.extend(sep),
                        None => st.end = Some(sep),
                    },
                    Token::Flow { end, .. } => end.extend(sep),
                    _ => {}
                }
            } else {
                it.start.extend(sep);
            }
        }
    }
}

pub(super) struct Parser {
    at_new_line: bool,
    at_scalar: bool,
    indent: i64,
    offset: i64,
    on_key_line: bool,
    stack: Vec<Token>,
    source: String,
    ty: &'static str,
    pub line_starts: Vec<i64>,
    out: Vec<Token>,
}

impl Parser {
    /// Parse a complete YAML stream; returns the top-level tokens and the line start offsets.
    pub(super) fn parse(src: &[u16]) -> (Vec<Token>, Vec<i64>) {
        let mut parser = Parser {
            at_new_line: true,
            at_scalar: false,
            indent: 0,
            offset: 0,
            on_key_line: false,
            stack: Vec::new(),
            source: String::new(),
            ty: "",
            line_starts: vec![0],
            out: Vec::new(),
        };
        for lexeme in Lexer::lex(src) {
            parser.next(lexeme);
        }
        while !parser.stack.is_empty() {
            parser.pop(None);
        }
        (parser.out, parser.line_starts)
    }

    fn on_new_line(&mut self, offset: i64) {
        self.line_starts.push(offset);
    }

    fn next(&mut self, source: String) {
        let len = ulen(&source);
        self.source = source;
        if self.at_scalar {
            self.at_scalar = false;
            self.step();
            self.offset += len;
            return;
        }
        let Some(ty) = token_type(&self.source) else {
            let message = format!("Not a YAML token: {}", self.source);
            let err = Token::Error {
                offset: self.offset,
                message,
                source: self.source.clone(),
            };
            self.pop(Some(err));
            self.offset += len;
            return;
        };
        if ty == "scalar" {
            self.at_new_line = false;
            self.at_scalar = true;
            self.ty = "scalar";
            return;
        }
        self.ty = ty;
        self.step();
        match ty {
            "newline" => {
                self.at_new_line = true;
                self.indent = 0;
                self.on_new_line(self.offset + len);
            }
            "space" => {
                if self.at_new_line && self.source.starts_with(' ') {
                    self.indent += len;
                }
            }
            "explicit-key-ind" | "map-value-ind" | "seq-item-ind" => {
                if self.at_new_line {
                    self.indent += len;
                }
            }
            "doc-mode" | "flow-error-end" => return,
            _ => self.at_new_line = false,
        }
        self.offset += len;
    }

    fn source_token(&self) -> St {
        St {
            ty: self.ty,
            offset: self.offset,
            indent: self.indent,
            source: self.source.clone(),
            end: None,
        }
    }

    fn st(&self) -> Token {
        Token::Src(self.source_token())
    }

    fn top_ty(&self) -> Option<&'static str> {
        self.stack.last().map(|t| t.ty())
    }

    fn step(&mut self) {
        let top = self.top_ty();
        if self.ty == "doc-end" && top != Some("doc-end") {
            while !self.stack.is_empty() {
                self.pop(None);
            }
            self.stack.push(Token::DocEnd {
                offset: self.offset,
                source: self.source.clone(),
                end: None,
            });
            return;
        }
        let Some(top) = top else {
            return self.stream();
        };
        match top {
            "document" => self.document(),
            "alias" | "scalar" | "single-quoted-scalar" | "double-quoted-scalar" => self.scalar(),
            "block-scalar" => self.block_scalar(),
            "block-map" => self.block_map(),
            "block-seq" => self.block_sequence(),
            "flow-collection" => self.flow_collection(),
            "doc-end" => self.document_end(),
            _ => self.pop(None),
        }
    }

    fn pop(&mut self, error: Option<Token>) {
        let token = match error {
            Some(e) => Some(e),
            None => self.stack.pop(),
        };
        let Some(mut token) = token else {
            self.out.push(Token::Error {
                offset: self.offset,
                source: String::new(),
                message: "Tried to pop an empty stack".to_string(),
            });
            return;
        };
        if self.stack.is_empty() {
            self.out.push(token);
            return;
        }
        let top_ty = self.top_ty().unwrap_or("");
        let top_indent = self.stack.last().and_then(|t| t.indent());
        if token.ty() == "block-scalar" {
            // Block scalars use their parent rather than header indent
            token.set_indent(top_indent.unwrap_or(0));
        } else if token.ty() == "flow-collection" && top_ty == "document" {
            // Ignore all indent for top-level flow collections
            token.set_indent(0);
        }
        if token.ty() == "flow-collection" {
            fix_flow_seq_items(&mut token);
        }
        // The trailing-empty-item move happens after the token is attached in the original;
        // it only depends on the token and the parent type, so compute it first.
        let moved_start = |token: &mut Token| -> Option<Vec<Token>> {
            if !matches!(top_ty, "document" | "block-map" | "block-seq") {
                return None;
            }
            let (indent, items) = match token {
                Token::BlockMap { indent, items, .. } | Token::BlockSeq { indent, items, .. } => {
                    (*indent, items)
                }
                _ => return None,
            };
            let last = items.last()?;
            if last.sep.is_none()
                && last.value.is_none()
                && !last.start.is_empty()
                && find_non_empty_index(&last.start) == -1
                && (indent == 0
                    || last.start.iter().all(|st| {
                        st.ty() != "comment" || st.indent().unwrap_or(0) < indent
                    }))
            {
                return items.pop().map(|it| it.start);
            }
            None
        };
        if !matches!(top_ty, "document" | "block-scalar" | "block-map" | "block-seq" | "flow-collection") {
            self.pop(None);
            self.pop(Some(token));
            return;
        }
        let top = self.stack.last_mut().unwrap();
        match top {
            Token::Document { value, end, .. } => {
                let moved = moved_start(&mut token);
                *value = Some(Box::new(token));
                if let Some(start) = moved {
                    *end = Some(start);
                }
            }
            Token::BlockScalar { props, .. } => props.push(token), // error
            Token::BlockMap { items, .. } => {
                let n = items.len();
                if items[n - 1].value.is_some() {
                    items.push(Item {
                        key: Some(Box::new(token)),
                        sep: Some(Vec::new()),
                        ..Default::default()
                    });
                    self.on_key_line = true;
                } else if items[n - 1].sep.is_some() {
                    let moved = moved_start(&mut token);
                    items[n - 1].value = Some(Box::new(token));
                    if let Some(start) = moved {
                        items.push(Item { start, ..Default::default() });
                    }
                } else {
                    let it = &mut items[n - 1];
                    it.key = Some(Box::new(token));
                    it.sep = Some(Vec::new());
                    self.on_key_line = !it.explicit_key;
                }
            }
            Token::BlockSeq { items, .. } => {
                let moved = moved_start(&mut token);
                let n = items.len();
                if items[n - 1].value.is_some() {
                    items.push(Item {
                        value: Some(Box::new(token)),
                        ..Default::default()
                    });
                } else {
                    items[n - 1].value = Some(Box::new(token));
                }
                if let Some(start) = moved {
                    items.push(Item { start, ..Default::default() });
                }
            }
            Token::Flow { items, .. } => {
                match items.last_mut() {
                    Some(it) if it.value.is_none() => {
                        if it.sep.is_some() {
                            it.value = Some(Box::new(token));
                        } else {
                            it.key = Some(Box::new(token));
                            it.sep = Some(Vec::new());
                        }
                    }
                    _ => items.push(Item {
                        key: Some(Box::new(token)),
                        sep: Some(Vec::new()),
                        ..Default::default()
                    }),
                }
            }
            _ => {}
        }
    }

    fn stream(&mut self) {
        match self.ty {
            "directive-line" => {
                self.out.push(Token::Directive {
                    offset: self.offset,
                    source: self.source.clone(),
                });
                return;
            }
            "byte-order-mark" | "space" | "comment" | "newline" => {
                let st = self.st();
                self.out.push(st);
                return;
            }
            "doc-mode" | "doc-start" => {
                let mut start = Vec::new();
                if self.ty == "doc-start" {
                    start.push(self.st());
                }
                self.stack.push(Token::Document {
                    offset: self.offset,
                    start,
                    value: None,
                    end: None,
                });
                return;
            }
            _ => {}
        }
        self.out.push(Token::Error {
            offset: self.offset,
            message: format!("Unexpected {} token in YAML stream", self.ty),
            source: self.source.clone(),
        });
    }

    fn document(&mut self) {
        let st = self.st();
        let has_value = matches!(self.stack.last(), Some(Token::Document { value: Some(_), .. }));
        if has_value {
            return self.line_end();
        }
        match self.ty {
            "doc-start" => {
                let non_empty = match self.stack.last() {
                    Some(Token::Document { start, .. }) => find_non_empty_index(start) != -1,
                    _ => false,
                };
                if non_empty {
                    self.pop(None);
                    self.step();
                } else if let Some(Token::Document { start, .. }) = self.stack.last_mut() {
                    start.push(st);
                }
                return;
            }
            "anchor" | "tag" | "space" | "comment" | "newline" => {
                if let Some(Token::Document { start, .. }) = self.stack.last_mut() {
                    start.push(st);
                }
                return;
            }
            _ => {}
        }
        if let Some(bv) = self.start_block_value() {
            self.stack.push(bv);
        } else {
            self.out.push(Token::Error {
                offset: self.offset,
                message: format!("Unexpected {} token in YAML document", self.ty),
                source: self.source.clone(),
            });
        }
    }

    fn scalar(&mut self) {
        if self.ty == "map-value-ind" {
            let mut scalar = self.stack.pop().unwrap();
            let start = {
                let parent = self.stack.last_mut();
                get_first_key_start_props(parent.and_then(get_prev_props))
            };
            let st = self.st();
            let sep = match &mut scalar {
                Token::Src(s) => match s.end.take() {
                    Some(mut end) => {
                        end.push(st);
                        end
                    }
                    None => vec![st],
                },
                _ => vec![st],
            };
            let map = Token::BlockMap {
                offset: scalar.offset(),
                indent: scalar.indent().unwrap_or(0),
                items: vec![Item {
                    start,
                    key: Some(Box::new(scalar)),
                    sep: Some(sep),
                    ..Default::default()
                }],
            };
            self.on_key_line = true;
            self.stack.push(map);
        } else {
            self.line_end();
        }
    }

    fn block_scalar(&mut self) {
        let st = self.st();
        match self.ty {
            "space" | "comment" | "newline" => {
                if let Some(Token::BlockScalar { props, .. }) = self.stack.last_mut() {
                    props.push(st);
                }
            }
            "scalar" => {
                if let Some(Token::BlockScalar { source, .. }) = self.stack.last_mut() {
                    *source = self.source.clone();
                }
                // block-scalar source includes trailing newline
                self.at_new_line = true;
                self.indent = 0;
                self.new_lines_in_source();
                self.pop(None);
            }
            _ => {
                self.pop(None);
                self.step();
            }
        }
    }

    /// The `onNewLine` calls for newlines inside the current token's source.
    fn new_lines_in_source(&mut self) {
        let units: Vec<u16> = self.source.encode_utf16().collect();
        for (i, c) in units.iter().enumerate() {
            if *c == b'\n' as u16 {
                self.on_new_line(self.offset + i as i64 + 1);
            }
        }
    }


    fn block_map(&mut self) {
        let st = self.st();
        let ty = self.ty;
        let this_indent = self.indent;
        let this_offset = self.offset;
        let on_key_line = self.on_key_line;
        let Some(Token::BlockMap { indent: map_indent, items, .. }) = self.stack.last_mut() else {
            return;
        };
        let map_indent = *map_indent;
        // it.sep is true-ish if pair already has key or : separator
        match ty {
            "newline" => {
                self.on_key_line = false;
                let n = items.len();
                if items[n - 1].value.is_some() {
                    let value = items[n - 1].value.as_mut().unwrap();
                    let last_is_comment = value
                        .end_array()
                        .and_then(|e| e.last())
                        .is_some_and(|t| t.ty() == "comment");
                    if last_is_comment {
                        value.end_array_mut().unwrap().push(st);
                    } else {
                        items.push(Item { start: vec![st], ..Default::default() });
                    }
                } else if let Some(sep) = items[n - 1].sep.as_mut() {
                    sep.push(st);
                } else {
                    items[n - 1].start.push(st);
                }
                return;
            }
            "space" | "comment" => {
                let n = items.len();
                let it = &items[n - 1];
                if it.value.is_some() {
                    items.push(Item { start: vec![st], ..Default::default() });
                } else if it.sep.is_some() {
                    items[n - 1].sep.as_mut().unwrap().push(st);
                } else {
                    let indented = ty == "comment"
                        && this_indent > map_indent
                        && it.start.iter().all(|t| t.ty() == "newline" || t.ty() == "space");
                    if indented && n >= 2 && items[n - 2].value.as_ref().and_then(|v| v.end_array()).is_some()
                    {
                        let start = std::mem::take(&mut items[n - 1].start);
                        let end = items[n - 2].value.as_mut().unwrap().end_array_mut().unwrap();
                        end.extend(start);
                        end.push(st);
                        items.pop();
                        return;
                    }
                    items[n - 1].start.push(st);
                }
                return;
            }
            _ => {}
        }
        if this_indent >= map_indent {
            let n = items.len();
            let at_map_indent = !on_key_line && this_indent == map_indent;
            let at_next_item = {
                let it = &items[n - 1];
                at_map_indent && (it.sep.is_some() || it.explicit_key) && ty != "seq-item-ind"
            };
            // For empty nodes, assign newline-separated not indented empty tokens to following node
            let mut start: Vec<Token> = Vec::new();
            {
                let it = &mut items[n - 1];
                if at_next_item
                    && it.value.is_none()
                    && let Some(sep) = it.sep.as_mut()
                {
                    {
                        let mut nl: Vec<usize> = Vec::new();
                        for (i, st) in sep.iter().enumerate() {
                            match st.ty() {
                                "newline" => nl.push(i),
                                "space" => {}
                                "comment" => {
                                    if st.indent().unwrap_or(0) > map_indent {
                                        nl.clear();
                                    }
                                }
                                _ => nl.clear(),
                            }
                        }
                        if nl.len() >= 2 {
                            start = sep.split_off(nl[1]);
                        }
                    }
                }
            }
            match ty {
                "anchor" | "tag" => {
                    if at_next_item || items[n - 1].value.is_some() {
                        start.push(st);
                        items.push(Item { start, ..Default::default() });
                        self.on_key_line = true;
                    } else if let Some(sep) = items[n - 1].sep.as_mut() {
                        sep.push(st);
                    } else {
                        items[n - 1].start.push(st);
                    }
                    return;
                }
                "explicit-key-ind" => {
                    if items[n - 1].sep.is_none() && !items[n - 1].explicit_key {
                        items[n - 1].start.push(st);
                        items[n - 1].explicit_key = true;
                    } else if at_next_item || items[n - 1].value.is_some() {
                        start.push(st);
                        items.push(Item { start, explicit_key: true, ..Default::default() });
                    } else {
                        self.stack.push(Token::BlockMap {
                            offset: this_offset,
                            indent: this_indent,
                            items: vec![Item { start: vec![st], explicit_key: true, ..Default::default() }],
                        });
                    }
                    self.on_key_line = true;
                    return;
                }
                "map-value-ind" => {
                    let mut push: Option<Token> = None;
                    let explicit_key = items[n - 1].explicit_key;
                    let has_sep = items[n - 1].sep.is_some();
                    let has_value = items[n - 1].value.is_some();
                    let sep_has = |items: &Vec<Item>, ty: &str| {
                        items[n - 1].sep.as_ref().is_some_and(|s| includes_token(s, ty))
                    };
                    if explicit_key {
                        if !has_sep {
                            if includes_token(&items[n - 1].start, "newline") {
                                items[n - 1].key = None;
                                items[n - 1].sep = Some(vec![st]);
                            } else {
                                let start = get_first_key_start_props(Some(&mut items[n - 1].start));
                                push = Some(Token::BlockMap {
                                    offset: this_offset,
                                    indent: this_indent,
                                    items: vec![Item { start, sep: Some(vec![st]), ..Default::default() }],
                                });
                            }
                        } else if has_value {
                            items.push(Item { sep: Some(vec![st]), ..Default::default() });
                        } else if sep_has(items, "map-value-ind") {
                            push = Some(Token::BlockMap {
                                offset: this_offset,
                                indent: this_indent,
                                items: vec![Item { start, sep: Some(vec![st]), ..Default::default() }],
                            });
                        } else if is_flow_token(items[n - 1].key.as_deref()) && !sep_has(items, "newline") {
                            let it = &mut items[n - 1];
                            let start = get_first_key_start_props(Some(&mut it.start));
                            let key = it.key.take();
                            let mut sep = it.sep.take().unwrap();
                            sep.push(st);
                            push = Some(Token::BlockMap {
                                offset: this_offset,
                                indent: this_indent,
                                items: vec![Item { start, key, sep: Some(sep), ..Default::default() }],
                            });
                        } else if !start.is_empty() {
                            // Not actually at next item
                            let sep = items[n - 1].sep.as_mut().unwrap();
                            sep.extend(start);
                            sep.push(st);
                        } else {
                            items[n - 1].sep.as_mut().unwrap().push(st);
                        }
                    } else if !has_sep {
                        items[n - 1].key = None;
                        items[n - 1].sep = Some(vec![st]);
                    } else if has_value || at_next_item {
                        items.push(Item { start, sep: Some(vec![st]), ..Default::default() });
                    } else if sep_has(items, "map-value-ind") {
                        push = Some(Token::BlockMap {
                            offset: this_offset,
                            indent: this_indent,
                            items: vec![Item { sep: Some(vec![st]), ..Default::default() }],
                        });
                    } else {
                        items[n - 1].sep.as_mut().unwrap().push(st);
                    }
                    if let Some(p) = push {
                        self.stack.push(p);
                    }
                    self.on_key_line = true;
                    return;
                }
                "alias" | "scalar" | "single-quoted-scalar" | "double-quoted-scalar" => {
                    let it_has_value = items[n - 1].value.is_some();
                    let it_has_sep = items[n - 1].sep.is_some();
                    let fs = self.flow_scalar();
                    let Some(Token::BlockMap { items, .. }) = self.stack.last_mut() else { return };
                    if at_next_item || it_has_value {
                        items.push(Item { start, key: Some(Box::new(fs)), sep: Some(Vec::new()), ..Default::default() });
                        self.on_key_line = true;
                    } else if it_has_sep {
                        self.stack.push(fs);
                    } else {
                        let it = items.last_mut().unwrap();
                        it.key = Some(Box::new(fs));
                        it.sep = Some(Vec::new());
                        self.on_key_line = true;
                    }
                    return;
                }
                _ => {
                    let (explicit_key, sep_no_newline) = {
                        let it = &items[n - 1];
                        (
                            it.explicit_key,
                            it.sep.as_ref().map(|s| !includes_token(s, "newline")),
                        )
                    };
                    if let Some(bv) = self.start_block_value() {
                        if bv.ty() == "block-seq" {
                            if !explicit_key && sep_no_newline == Some(true) {
                                let err = Token::Error {
                                    offset: self.offset,
                                    message: "Unexpected block-seq-ind on same line with key".to_string(),
                                    source: self.source.clone(),
                                };
                                self.pop(Some(err));
                                return;
                            }
                        } else if at_map_indent
                            && let Some(Token::BlockMap { items, .. }) = self.stack.last_mut()
                        {
                            items.push(Item { start, ..Default::default() });
                        }
                        self.stack.push(bv);
                        return;
                    }
                }
            }
        }
        self.pop(None);
        self.step();
    }

    fn block_sequence(&mut self) {
        let st = self.st();
        let ty = self.ty;
        let this_indent = self.indent;
        let Some(Token::BlockSeq { indent: seq_indent, items, .. }) = self.stack.last_mut() else {
            return;
        };
        let seq_indent = *seq_indent;
        let n = items.len();
        match ty {
            "newline" => {
                if items[n - 1].value.is_some() {
                    let value = items[n - 1].value.as_mut().unwrap();
                    let last_is_comment = value
                        .end_array()
                        .and_then(|e| e.last())
                        .is_some_and(|t| t.ty() == "comment");
                    if last_is_comment {
                        value.end_array_mut().unwrap().push(st);
                    } else {
                        items.push(Item { start: vec![st], ..Default::default() });
                    }
                } else {
                    items[n - 1].start.push(st);
                }
                return;
            }
            "space" | "comment" => {
                if items[n - 1].value.is_some() {
                    items.push(Item { start: vec![st], ..Default::default() });
                } else {
                    let indented = ty == "comment"
                        && this_indent > seq_indent
                        && items[n - 1].start.iter().all(|t| t.ty() == "newline" || t.ty() == "space");
                    if indented && n >= 2 && items[n - 2].value.as_ref().and_then(|v| v.end_array()).is_some()
                    {
                        let start = std::mem::take(&mut items[n - 1].start);
                        let end = items[n - 2].value.as_mut().unwrap().end_array_mut().unwrap();
                        end.extend(start);
                        end.push(st);
                        items.pop();
                        return;
                    }
                    items[n - 1].start.push(st);
                }
                return;
            }
            "anchor" | "tag" => {
                if !(items[n - 1].value.is_some() || this_indent <= seq_indent) {
                    items[n - 1].start.push(st);
                    return;
                }
            }
            "seq-item-ind" if this_indent == seq_indent => {
                if items[n - 1].value.is_some() || includes_token(&items[n - 1].start, "seq-item-ind") {
                    items.push(Item { start: vec![st], ..Default::default() });
                } else {
                    items[n - 1].start.push(st);
                }
                return;
            }
            _ => {}
        }
        if this_indent > seq_indent
            && let Some(bv) = self.start_block_value()
        {
            self.stack.push(bv);
            return;
        }
        self.pop(None);
        self.step();
    }

    fn flow_collection(&mut self) {
        let st = self.st();
        if self.ty == "flow-error-end" {
            loop {
                self.pop(None);
                if self.top_ty() != Some("flow-collection") {
                    break;
                }
            }
            return;
        }
        let Some(Token::Flow { items, end, .. }) = self.stack.last_mut() else { return };
        if end.is_empty() {
            let it_state = items.last().map(|it| (it.sep.is_some(), it.value.is_some()));
            match self.ty {
                "comma" | "explicit-key-ind" => {
                    match it_state {
                        None | Some((true, _)) => items.push(Item { start: vec![st], ..Default::default() }),
                        _ => items.last_mut().unwrap().start.push(st),
                    }
                    return;
                }
                "map-value-ind" => {
                    match it_state {
                        None | Some((_, true)) => {
                            items.push(Item { sep: Some(vec![st]), ..Default::default() })
                        }
                        Some((true, false)) => items.last_mut().unwrap().sep.as_mut().unwrap().push(st),
                        Some((false, false)) => {
                            let it = items.last_mut().unwrap();
                            it.key = None;
                            it.sep = Some(vec![st]);
                        }
                    }
                    return;
                }
                "space" | "comment" | "newline" | "anchor" | "tag" => {
                    match it_state {
                        None | Some((_, true)) => items.push(Item { start: vec![st], ..Default::default() }),
                        Some((true, false)) => items.last_mut().unwrap().sep.as_mut().unwrap().push(st),
                        Some((false, false)) => items.last_mut().unwrap().start.push(st),
                    }
                    return;
                }
                "alias" | "scalar" | "single-quoted-scalar" | "double-quoted-scalar" => {
                    let fs = self.flow_scalar();
                    let Some(Token::Flow { items, .. }) = self.stack.last_mut() else { return };
                    match it_state {
                        None | Some((_, true)) => items.push(Item {
                            key: Some(Box::new(fs)),
                            sep: Some(Vec::new()),
                            ..Default::default()
                        }),
                        Some((true, false)) => self.stack.push(fs),
                        Some((false, false)) => {
                            let it = items.last_mut().unwrap();
                            it.key = Some(Box::new(fs));
                            it.sep = Some(Vec::new());
                        }
                    }
                    return;
                }
                "flow-map-end" | "flow-seq-end" => {
                    end.push(st);
                    return;
                }
                _ => {}
            }
            if let Some(bv) = self.start_block_value() {
                self.stack.push(bv);
            } else {
                self.pop(None);
                self.step();
            }
        } else {
            let n = self.stack.len();
            let fc_indent = self.stack[n - 1].indent().unwrap_or(0);
            let parent = n.checked_sub(2).map(|i| &self.stack[i]);
            let parent_ty = parent.map(|p| p.ty()).unwrap_or("");
            let parent_cond = match parent {
                Some(Token::BlockMap { indent, items, .. }) => {
                    (self.ty == "map-value-ind" && *indent == fc_indent)
                        || (self.ty == "newline" && items.last().is_some_and(|it| it.sep.is_none()))
                }
                _ => false,
            };
            if parent_cond {
                self.pop(None);
                self.step();
            } else if self.ty == "map-value-ind" && parent_ty != "flow-collection" {
                let mut fc = self.stack.pop().unwrap();
                let start = get_first_key_start_props(self.stack.last_mut().and_then(get_prev_props));
                fix_flow_seq_items(&mut fc);
                let mut sep = match &mut fc {
                    Token::Flow { end, .. } => {
                        if end.len() > 1 { end.split_off(1) } else { Vec::new() }
                    }
                    _ => Vec::new(),
                };
                sep.push(st);
                let map = Token::BlockMap {
                    offset: fc.offset(),
                    indent: fc.indent().unwrap_or(0),
                    items: vec![Item {
                        start,
                        key: Some(Box::new(fc)),
                        sep: Some(sep),
                        ..Default::default()
                    }],
                };
                self.on_key_line = true;
                self.stack.push(map);
            } else {
                self.line_end();
            }
        }
    }

    fn flow_scalar(&mut self) -> Token {
        self.new_lines_in_source();
        Token::Src(St {
            ty: self.ty,
            offset: self.offset,
            indent: self.indent,
            source: self.source.clone(),
            end: None,
        })
    }

    /// `startBlockValue(parent)` with the stack top as the parent.
    fn start_block_value(&mut self) -> Option<Token> {
        match self.ty {
            "alias" | "scalar" | "single-quoted-scalar" | "double-quoted-scalar" => {
                Some(self.flow_scalar())
            }
            "block-scalar-header" => Some(Token::BlockScalar {
                offset: self.offset,
                indent: self.indent,
                props: vec![self.st()],
                source: String::new(),
            }),
            "flow-map-start" | "flow-seq-start" => Some(Token::Flow {
                offset: self.offset,
                indent: self.indent,
                start: self.source_token(),
                items: Vec::new(),
                end: Vec::new(),
            }),
            "seq-item-ind" => Some(Token::BlockSeq {
                offset: self.offset,
                indent: self.indent,
                items: vec![Item { start: vec![self.st()], ..Default::default() }],
            }),
            "explicit-key-ind" => {
                self.on_key_line = true;
                let st = self.st();
                let mut start = get_first_key_start_props(self.stack.last_mut().and_then(get_prev_props));
                start.push(st);
                Some(Token::BlockMap {
                    offset: self.offset,
                    indent: self.indent,
                    items: vec![Item { start, explicit_key: true, ..Default::default() }],
                })
            }
            "map-value-ind" => {
                self.on_key_line = true;
                let st = self.st();
                let start = get_first_key_start_props(self.stack.last_mut().and_then(get_prev_props));
                Some(Token::BlockMap {
                    offset: self.offset,
                    indent: self.indent,
                    items: vec![Item { start, sep: Some(vec![st]), ..Default::default() }],
                })
            }
            _ => None,
        }
    }

    fn document_end(&mut self) {
        if self.ty != "doc-mode" {
            let st = self.st();
            if let Some(top) = self.stack.last_mut() {
                top.push_end(st);
            }
            if self.ty == "newline" {
                self.pop(None);
            }
        }
    }

    fn line_end(&mut self) {
        match self.ty {
            "comma" | "doc-start" | "doc-end" | "flow-seq-end" | "flow-map-end" | "map-value-ind" => {
                self.pop(None);
                self.step();
            }
            _ => {
                if self.ty == "newline" {
                    self.on_key_line = false;
                }
                let st = self.st();
                if let Some(top) = self.stack.last_mut() {
                    top.push_end(st);
                }
                if self.ty == "newline" {
                    self.pop(None);
                }
            }
        }
    }
}

fn push_item(it: Item, out: &mut Vec<Token>) {
    out.extend(it.start);
    if let Some(k) = it.key {
        out.push(*k);
    }
    if let Some(sep) = it.sep {
        out.extend(sep);
    }
    if let Some(v) = it.value {
        out.push(*v);
    }
}

/// Move the children of `token` into `out` (iterative drop of deep trees).
pub(super) fn take_children(token: Token, out: &mut Vec<Token>) {
    match token {
        Token::Src(st) => out.extend(st.end.unwrap_or_default()),
        Token::BlockScalar { props, .. } => out.extend(props),
        Token::BlockMap { items, .. } | Token::BlockSeq { items, .. } => {
            for it in items {
                push_item(it, out);
            }
        }
        Token::Flow { items, end, .. } => {
            for it in items {
                push_item(it, out);
            }
            out.extend(end);
        }
        Token::Document { start, value, end, .. } => {
            out.extend(start);
            if let Some(v) = value {
                out.push(*v);
            }
            out.extend(end.unwrap_or_default());
        }
        Token::DocEnd { end, .. } => out.extend(end.unwrap_or_default()),
        Token::Directive { .. } | Token::Error { .. } => {}
    }
}
