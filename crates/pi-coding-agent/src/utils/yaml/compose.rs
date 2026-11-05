//! Port of `yaml` 2.9.0 `dist/compose/*` (composer, compose-doc/node/collection/scalar,
//! resolve-props/end/block-map/block-seq/flow-collection/block-scalar/flow-scalar, utils),
//! `dist/doc/directives.js` and the core schema tags (`dist/schema/{common,core}` plus the known
//! tags `binary`, `merge`, `omap`, `pairs`, `set`, `timestamp`).
//!
//! Only what `YAML.parse(src)` with default options needs: comments are tracked only as far as
//! they influence values (`!!set` membership) and warnings are dropped (pi silences
//! `process.emitWarning`).

use super::parser::{Item, St, Token, ulen};

pub(super) type NodeId = usize;

/// Nesting depth at which composition reports a stack overflow (V8 overflows at a similar
/// depth; the exact limit depends on the JavaScript engine's stack).
pub(super) const MAX_COMPOSE_DEPTH: usize = 800;

const STACK_OVERFLOW: &str = "Maximum call stack size exceeded";

#[derive(Debug, Clone)]
pub(super) enum SVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Buffer(Vec<u8>),
    Date(f64),
    /// `Symbol('<<')` of an explicit `!!merge` scalar.
    Symbol,
}

impl SVal {
    /// JavaScript `a === b` on scalar values (objects and symbols are distinct).
    fn strict_eq(&self, other: &SVal) -> bool {
        match (self, other) {
            (SVal::Null, SVal::Null) => true,
            (SVal::Bool(a), SVal::Bool(b)) => a == b,
            (SVal::Num(a), SVal::Num(b)) => a == b,
            (SVal::Str(a), SVal::Str(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MapClass {
    Map,
    Set,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeqClass {
    Seq,
    OMap,
}

#[derive(Debug, Clone)]
pub(super) struct Pair {
    pub key: NodeId,
    pub value: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub(super) enum SeqItem {
    Node(NodeId),
    Pair(Pair),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScalarType {
    Plain,
    QuoteSingle,
    QuoteDouble,
    BlockLiteral,
    BlockFolded,
}

#[derive(Debug, Clone)]
pub(super) enum Kind {
    Scalar {
        value: SVal,
        ty: Option<ScalarType>,
        /// `scalar.source` (the resolved string before tag resolution).
        source: String,
        /// The scalar has `addToJSMap` (explicit `!!merge`).
        merge: bool,
    },
    Alias {
        source: String,
    },
    Map {
        items: Vec<Pair>,
        class: MapClass,
    },
    Seq {
        items: Vec<SeqItem>,
        class: SeqClass,
    },
}

#[derive(Debug, Clone)]
pub(super) struct Node {
    pub kind: Kind,
    pub anchor: Option<String>,
    pub tag: Option<String>,
    pub range: [i64; 3],
    /// `node.comment` or `node.commentBefore` is set.
    pub comment: bool,
}

#[derive(Debug, Clone)]
pub(super) struct YamlErr {
    pub pos: (i64, i64),
    pub message: String,
}

pub(super) struct Doc {
    pub contents: Option<NodeId>,
    pub errors: Vec<YamlErr>,
    pub range: [i64; 3],
}

const DEFAULT_TAG_HANDLE: (&str, &str) = ("!!", "tag:yaml.org,2002:");
const MAP_TAG: &str = "tag:yaml.org,2002:map";
const SEQ_TAG: &str = "tag:yaml.org,2002:seq";
const STR_TAG: &str = "tag:yaml.org,2002:str";

#[derive(Debug, Clone)]
struct Directives {
    version: String,
    explicit: bool,
    tags: Vec<(String, String)>,
    at_next_document: bool,
}

impl Directives {
    fn new() -> Self {
        Directives {
            version: "1.2".to_string(),
            explicit: false,
            tags: vec![(DEFAULT_TAG_HANDLE.0.to_string(), DEFAULT_TAG_HANDLE.1.to_string())],
            at_next_document: false,
        }
    }

    fn set_tag(&mut self, handle: &str, prefix: &str) {
        if let Some(entry) = self.tags.iter_mut().find(|(h, _)| h == handle) {
            entry.1 = prefix.to_string();
        } else {
            self.tags.push((handle.to_string(), prefix.to_string()));
        }
    }

    fn at_document(&mut self) -> Directives {
        let res = Directives {
            version: self.version.clone(),
            explicit: self.explicit,
            tags: self.tags.clone(),
            at_next_document: false,
        };
        match self.version.as_str() {
            "1.1" => self.at_next_document = true,
            "1.2" => {
                self.at_next_document = false;
                self.explicit = false;
                self.version = "1.2".to_string();
                self.tags = Directives::new().tags;
            }
            _ => {}
        }
        res
    }

    /// `add(line, onError)`; errors are `(offset, message, warning)`.
    fn add(&mut self, line: &str, errors: &mut Vec<(i64, String, bool)>) {
        if self.at_next_document {
            self.explicit = false;
            self.version = "1.1".to_string();
            self.tags = Directives::new().tags;
            self.at_next_document = false;
        }
        let trimmed = js_trim(line);
        let mut parts: Vec<&str> = trimmed.split([' ', '\t']).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            parts.push("");
        }
        let name = parts.remove(0);
        match name {
            "%TAG" => {
                if parts.len() != 2 {
                    errors.push((0, "%TAG directive should contain exactly two parts".into(), false));
                    if parts.len() < 2 {
                        return;
                    }
                }
                let (handle, prefix) = (parts[0], parts[1]);
                self.set_tag(handle, prefix);
            }
            "%YAML" => {
                self.explicit = true;
                if parts.len() != 1 {
                    errors.push((0, "%YAML directive should contain exactly one part".into(), false));
                    return;
                }
                let version = parts[0];
                if version == "1.1" || version == "1.2" {
                    self.version = version.to_string();
                } else {
                    let is_valid = version
                        .split_once('.')
                        .is_some_and(|(a, b)| {
                            !a.is_empty()
                                && !b.is_empty()
                                && a.bytes().all(|c| c.is_ascii_digit())
                                && b.bytes().all(|c| c.is_ascii_digit())
                        });
                    errors.push((6, format!("Unsupported YAML version {version}"), is_valid));
                }
            }
            _ => errors.push((0, format!("Unknown directive {name}"), true)),
        }
    }

    /// `tagName(source, onError)`; `Err` messages are reported, `Ok(None)` is `null`.
    fn tag_name(&self, source: &str, errors: &mut Vec<String>) -> Option<String> {
        if source == "!" {
            return Some("!".to_string()); // non-specific tag
        }
        if !source.starts_with('!') {
            errors.push(format!("Not a valid tag: {source}"));
            return None;
        }
        if source[1..].starts_with('<') {
            let units: Vec<u16> = source.encode_utf16().collect();
            let verbatim = if units.len() >= 3 {
                String::from_utf16_lossy(&units[2..units.len() - 1])
            } else {
                String::new()
            };
            if verbatim == "!" || verbatim == "!!" {
                errors.push(format!("Verbatim tags aren't resolved, so {source} is invalid."));
                return None;
            }
            if !source.ends_with('>') {
                errors.push("Verbatim tags must end with a >".to_string());
            }
            return Some(verbatim);
        }
        let split = source.rfind('!').unwrap_or(0) + 1;
        let (handle, suffix) = source.split_at(split);
        if suffix.is_empty() {
            errors.push(format!("The {source} tag has no suffix"));
        }
        if let Some((_, prefix)) = self.tags.iter().find(|(h, _)| h == handle)
            && !prefix.is_empty()
        {
            return match decode_uri_component(suffix) {
                Some(decoded) => Some(format!("{prefix}{decoded}")),
                None => {
                    errors.push("URIError: URI malformed".to_string());
                    None
                }
            };
        }
        if handle == "!" {
            return Some(source.to_string()); // local tag
        }
        errors.push(format!("Could not resolve tag: {source}"));
        None
    }
}

/// JavaScript `String.prototype.trim`.
pub(super) fn js_trim(s: &str) -> &str {
    fn ws(c: char) -> bool {
        matches!(
            c,
            '\t' | '\n'
                | '\u{b}'
                | '\u{c}'
                | '\r'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    }
    s.trim_matches(ws)
}

/// `decodeURIComponent`; `None` for a malformed sequence.
fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let hex = |b: u8| -> Option<u8> { (b as char).to_digit(16).map(|d| d as u8) };
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let byte_at = |i: usize| -> Option<u8> {
            if bytes.get(i) != Some(&b'%') {
                return None;
            }
            Some(hex(*bytes.get(i + 1)?)? * 16 + hex(*bytes.get(i + 2)?)?)
        };
        let b0 = byte_at(i)?;
        let n = if b0 < 0x80 {
            1
        } else if b0 & 0xE0 == 0xC0 {
            2
        } else if b0 & 0xF0 == 0xE0 {
            3
        } else if b0 & 0xF8 == 0xF0 {
            4
        } else {
            return None;
        };
        let mut seq = vec![b0];
        for k in 1..n {
            let b = byte_at(i + 3 * k)?;
            if b & 0xC0 != 0x80 {
                return None;
            }
            seq.push(b);
        }
        std::str::from_utf8(&seq).ok()?;
        out.extend(seq);
        i += 3 * n;
    }
    String::from_utf8(out).ok()
}

struct Props<'t> {
    comma: Option<&'t Token>,
    found: Option<&'t Token>,
    comment: String,
    has_newline: bool,
    anchor: Option<&'t Token>,
    tag: Option<&'t Token>,
    newline_after_prop: Option<&'t Token>,
    end: i64,
    start: i64,
}

/// `getErrorPos(token)`.
fn tok_pos(token: &Token) -> (i64, i64) {
    let offset = token.offset();
    (offset, offset + token.source().map(ulen).unwrap_or(1))
}

fn num_pos(n: i64) -> (i64, i64) {
    (n, n + 1)
}

fn range_pos(r: [i64; 3]) -> (i64, i64) {
    (r[0], r[1])
}

pub(super) struct Composer {
    pub nodes: Vec<Node>,
    errors: Vec<YamlErr>,
    directives: Directives,
    at_directives: bool,
    doc: Option<Doc>,
    pub docs: Vec<Doc>,
    // compose context
    doc_directives: Directives,
    at_key: bool,
    at_root: bool,
    depth: usize,
}

impl Composer {
    pub(super) fn compose(tokens: &[Token], end_offset: i64) -> Composer {
        let mut c = Composer {
            nodes: Vec::new(),
            errors: Vec::new(),
            directives: Directives::new(),
            at_directives: false,
            doc: None,
            docs: Vec::new(),
            doc_directives: Directives::new(),
            at_key: false,
            at_root: true,
            depth: 0,
        };
        for token in tokens {
            c.next(token);
        }
        c.end(end_offset);
        c
    }

    fn on_error(&mut self, pos: (i64, i64), message: impl Into<String>) {
        self.errors.push(YamlErr { pos, message: message.into() });
    }

    fn next(&mut self, token: &Token) {
        match token {
            Token::Directive { source, .. } => {
                let mut errs = Vec::new();
                self.directives.add(source, &mut errs);
                for (offset, message, warning) in errs {
                    if !warning {
                        let pos = tok_pos(token);
                        self.on_error((pos.0 + offset, pos.1), message);
                    }
                }
                self.at_directives = true;
            }
            Token::Document { .. } => {
                let doc = self.compose_doc(token);
                if self.at_directives && !doc.1 {
                    self.on_error(tok_pos(token), "Missing directives-end/doc-start indicator line");
                }
                let mut doc = doc.0;
                doc.errors = std::mem::take(&mut self.errors);
                if let Some(prev) = self.doc.take() {
                    self.docs.push(prev);
                }
                self.doc = Some(doc);
                self.at_directives = false;
            }
            Token::Src(st) if matches!(st.ty, "byte-order-mark" | "space" | "comment" | "newline") => {}
            Token::Error { message, source, .. } => {
                let msg = if source.is_empty() {
                    message.clone()
                } else {
                    format!("{message}: {}", pi_ai::utils::js_json::quote(source))
                };
                let error = YamlErr { pos: tok_pos(token), message: msg };
                match self.doc.as_mut() {
                    Some(doc) if !self.at_directives => doc.errors.push(error),
                    _ => self.errors.push(error),
                }
            }
            Token::DocEnd { offset, source, end } => {
                if self.doc.is_none() {
                    let msg = "Unexpected doc-end without preceding document";
                    self.errors.push(YamlErr { pos: tok_pos(token), message: msg.to_string() });
                    return;
                }
                let (_, end_offset) = self.resolve_end(end.as_deref(), offset + ulen(source), true);
                let errors = std::mem::take(&mut self.errors);
                let doc = self.doc.as_mut().unwrap();
                doc.errors.extend(errors);
                doc.range[2] = end_offset;
            }
            _ => {
                let msg = format!("Unsupported token {}", token.ty());
                self.errors.push(YamlErr { pos: tok_pos(token), message: msg });
            }
        }
    }

    fn end(&mut self, end_offset: i64) {
        if let Some(mut doc) = self.doc.take() {
            doc.errors.extend(std::mem::take(&mut self.errors));
            self.docs.push(doc);
        } else {
            if self.at_directives {
                self.on_error(num_pos(end_offset), "Missing directives-end indicator line");
            }
            let doc = Doc {
                contents: None,
                errors: std::mem::take(&mut self.errors),
                range: [0, end_offset, end_offset],
            };
            self.docs.push(doc);
        }
    }

    /// Returns the document and whether it had a doc-start marker.
    fn compose_doc(&mut self, token: &Token) -> (Doc, bool) {
        let Token::Document { offset, start, value, end } = token else { unreachable!() };
        self.doc_directives = self.directives.at_document();
        self.at_key = false;
        self.at_root = true;
        let value = value.as_deref();
        let next = value.or_else(|| end.as_ref().and_then(|e| e.first()));
        let props = self.resolve_props(start, None, "doc-start", next, *offset, 0, true);
        let mut doc_start = false;
        if props.found.is_some() {
            doc_start = true;
            if let Some(v) = value
                && matches!(v.ty(), "block-map" | "block-seq")
                && !props.has_newline
            {
                self.on_error(
                    num_pos(props.end),
                    "Block collection cannot start on same line with directives-end marker",
                );
            }
        }
        let contents = match value {
            Some(v) => self.compose_node(v, props),
            None => {
                let pe = props.end;
                self.compose_empty_node(pe, Some(start), props)
            }
        };
        let content_end = self.nodes[contents].range[2];
        let (_, re_offset) = self.resolve_end(end.as_deref(), content_end, false);
        (
            Doc {
                contents: Some(contents),
                errors: Vec::new(),
                range: [*offset, content_end, re_offset],
            },
            doc_start,
        )
    }

    /// `resolveEnd(end, offset, reqSpace, onError)` → `(comment, offset)`.
    fn resolve_end(&mut self, end: Option<&[Token]>, mut offset: i64, req_space: bool) -> (String, i64) {
        let mut comment = String::new();
        if let Some(end) = end {
            let mut has_space = false;
            let mut sep = String::new();
            for token in end {
                let source = token.source().unwrap_or("");
                match token.ty() {
                    "space" => has_space = true,
                    "comment" => {
                        if req_space && !has_space {
                            self.on_error(
                                tok_pos(token),
                                "Comments must be separated from other tokens by white space characters",
                            );
                        }
                        let cb = comment_body(source);
                        if comment.is_empty() {
                            comment = cb;
                        } else {
                            comment = format!("{comment}{sep}{cb}");
                        }
                        sep.clear();
                    }
                    "newline" => {
                        if !comment.is_empty() {
                            sep.push_str(source);
                        }
                        has_space = true;
                    }
                    ty => {
                        let msg = format!("Unexpected {ty} at node end");
                        self.on_error(tok_pos(token), msg);
                    }
                }
                offset += ulen(source);
            }
        }
        (comment, offset)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_props<'t>(
        &mut self,
        tokens: &'t [Token],
        flow: Option<&str>,
        indicator: &str,
        next: Option<&'t Token>,
        offset: i64,
        parent_indent: i64,
        start_on_newline: bool,
    ) -> Props<'t> {
        let mut at_newline = start_on_newline;
        let mut has_space = start_on_newline;
        let mut comment = String::new();
        let mut comment_sep = String::new();
        let mut has_newline = false;
        let mut req_space = false;
        let mut tab: Option<&Token> = None;
        let mut anchor: Option<&Token> = None;
        let mut tag: Option<&Token> = None;
        let mut newline_after_prop: Option<&Token> = None;
        let mut comma: Option<&Token> = None;
        let mut found: Option<&Token> = None;
        let mut start: Option<i64> = None;
        for token in tokens {
            let ty = token.ty();
            let source = token.source().unwrap_or("");
            if req_space {
                if ty != "space" && ty != "newline" && ty != "comma" {
                    self.on_error(
                        num_pos(token.offset()),
                        "Tags and anchors must be separated from the next token by white space",
                    );
                }
                req_space = false;
            }
            if let Some(t) = tab {
                if at_newline && ty != "comment" && ty != "newline" {
                    self.on_error(tok_pos(t), "Tabs are not allowed as indentation");
                }
                tab = None;
            }
            match ty {
                "space" => {
                    // At the doc level, tabs at line start may be parsed
                    // as leading white space rather than indentation.
                    // In a flow collection, only the parser handles indent.
                    if flow.is_none()
                        && (indicator != "doc-start" || next.map(|n| n.ty()) != Some("flow-collection"))
                        && source.contains('\t')
                    {
                        tab = Some(token);
                    }
                    has_space = true;
                }
                "comment" => {
                    if !has_space {
                        self.on_error(
                            tok_pos(token),
                            "Comments must be separated from other tokens by white space characters",
                        );
                    }
                    let cb = comment_body(source);
                    if comment.is_empty() {
                        comment = cb;
                    } else {
                        comment = format!("{comment}{comment_sep}{cb}");
                    }
                    comment_sep.clear();
                    at_newline = false;
                }
                "newline" => {
                    if at_newline {
                        // (`spaceBefore` only affects stringification)
                        if !comment.is_empty() {
                            comment.push_str(source);
                        }
                    } else {
                        comment_sep.push_str(source);
                    }
                    at_newline = true;
                    has_newline = true;
                    if anchor.is_some() || tag.is_some() {
                        newline_after_prop = Some(token);
                    }
                    has_space = true;
                }
                "anchor" => {
                    if anchor.is_some() {
                        self.on_error(tok_pos(token), "A node can have at most one anchor");
                    }
                    // 'Anchor ending in : is ambiguous' is a warning
                    anchor = Some(token);
                    start.get_or_insert(token.offset());
                    at_newline = false;
                    has_space = false;
                    req_space = true;
                }
                "tag" => {
                    if tag.is_some() {
                        self.on_error(tok_pos(token), "A node can have at most one tag");
                    }
                    tag = Some(token);
                    start.get_or_insert(token.offset());
                    at_newline = false;
                    has_space = false;
                    req_space = true;
                }
                t if t == indicator => {
                    // Could here handle preceding comments differently
                    if anchor.is_some() || tag.is_some() {
                        let msg = format!("Anchors and tags must be after the {source} indicator");
                        self.on_error(tok_pos(token), msg);
                    }
                    if found.is_some() {
                        let msg = format!("Unexpected {source} in {}", flow.unwrap_or("collection"));
                        self.on_error(tok_pos(token), msg);
                    }
                    found = Some(token);
                    at_newline = indicator == "seq-item-ind" || indicator == "explicit-key-ind";
                    has_space = false;
                }
                "comma" if flow.is_some() => {
                    if comma.is_some() {
                        let msg = format!("Unexpected , in {}", flow.unwrap());
                        self.on_error(tok_pos(token), msg);
                    }
                    comma = Some(token);
                    at_newline = false;
                    has_space = false;
                }
                _ => {
                    let msg = format!("Unexpected {ty} token");
                    self.on_error(tok_pos(token), msg);
                    at_newline = false;
                    has_space = false;
                }
            }
        }
        let end = match tokens.last() {
            Some(last) => last.offset() + last.source().map(ulen).unwrap_or(0),
            None => offset,
        };
        if req_space
            && let Some(next) = next
            && !matches!(next.ty(), "space" | "newline" | "comma")
            && (next.ty() != "scalar" || next.source() != Some(""))
        {
            self.on_error(
                num_pos(next.offset()),
                "Tags and anchors must be separated from the next token by white space",
            );
        }
        if let Some(t) = tab
            && ((at_newline && t.indent().unwrap_or(0) <= parent_indent)
                || matches!(next.map(|n| n.ty()), Some("block-map" | "block-seq")))
        {
            self.on_error(tok_pos(t), "Tabs are not allowed as indentation");
        }
        Props {
            comma,
            found,
            comment,
            has_newline,
            anchor,
            tag,
            newline_after_prop,
            end,
            start: start.unwrap_or(end),
        }
    }

    fn add_node(&mut self, kind: Kind, range: [i64; 3]) -> NodeId {
        self.nodes.push(Node { kind, anchor: None, tag: None, range, comment: false });
        self.nodes.len() - 1
    }

    fn compose_node(&mut self, token: &Token, props: Props<'_>) -> NodeId {
        let anchor = props.anchor;
        let tag = props.tag;
        let mut node: Option<NodeId> = None;
        match token.ty() {
            "alias" => {
                node = Some(self.compose_alias(token));
                if anchor.is_some() || tag.is_some() {
                    self.on_error(tok_pos(token), "An alias node must not specify any properties");
                }
            }
            "scalar" | "single-quoted-scalar" | "double-quoted-scalar" | "block-scalar" => {
                let id = self.compose_scalar(token, tag);
                if let Some(a) = anchor {
                    self.nodes[id].anchor = Some(anchor_name(a));
                }
                node = Some(id);
            }
            "block-map" | "block-seq" | "flow-collection" => {
                if self.depth >= MAX_COMPOSE_DEPTH {
                    // Almost certainly here due to a stack overflow
                    self.on_error(tok_pos(token), STACK_OVERFLOW);
                } else {
                    self.depth += 1;
                    let id = self.compose_collection(token, &props);
                    self.depth -= 1;
                    if let Some(a) = anchor {
                        self.nodes[id].anchor = Some(anchor_name(a));
                    }
                    node = Some(id);
                }
            }
            _ => {
                let message = match token {
                    Token::Error { message, .. } => message.clone(),
                    _ => format!("Unsupported token (type: {})", token.ty()),
                };
                self.on_error(tok_pos(token), message);
            }
        }
        let has_comment = !props.comment.is_empty();
        let node = match node {
            Some(n) => n,
            None => self.compose_empty_node(token.offset(), None, props),
        };
        if let Some(a) = anchor
            && self.nodes[node].anchor.as_deref() == Some("")
        {
            self.on_error(tok_pos(a), "Anchor cannot be an empty string");
        }
        if has_comment {
            self.nodes[node].comment = true;
        }
        node
    }

    /// `composeEmptyNode(ctx, offset, before, null, props, onError)`.
    fn compose_empty_node(&mut self, offset: i64, before: Option<&[Token]>, props: Props<'_>) -> NodeId {
        let token = Token::Src(St {
            ty: "scalar",
            offset: empty_scalar_position(offset, before),
            indent: -1,
            source: String::new(),
            end: None,
        });
        let node = self.compose_scalar(&token, props.tag);
        if let Some(a) = props.anchor {
            let name = anchor_name(a);
            let empty = name.is_empty();
            self.nodes[node].anchor = Some(name);
            if empty {
                self.on_error(tok_pos(a), "Anchor cannot be an empty string");
            }
        }
        if !props.comment.is_empty() {
            self.nodes[node].comment = true;
            self.nodes[node].range[2] = props.end;
        }
        node
    }

    fn compose_alias(&mut self, token: &Token) -> NodeId {
        let Token::Src(st) = token else { unreachable!() };
        let units: Vec<u16> = st.source.encode_utf16().collect();
        let source = String::from_utf16_lossy(units.get(1..).unwrap_or(&[]));
        if source.is_empty() {
            self.on_error(num_pos(st.offset), "Alias cannot be an empty string");
        }
        // 'Alias ending in : is ambiguous' is a warning
        let value_end = st.offset + units.len() as i64;
        let (comment, re_offset) = self.resolve_end(st.end.as_deref(), value_end, true);
        let id = self.add_node(Kind::Alias { source }, [st.offset, value_end, re_offset]);
        if !comment.is_empty() {
            self.nodes[id].comment = true;
        }
        id
    }

    fn compose_collection(&mut self, token: &Token, props: &Props<'_>) -> NodeId {
        let tag_token = props.tag;
        let tag_name = match tag_token {
            None => None,
            Some(t) => {
                let mut errs = Vec::new();
                let name = self.doc_directives.tag_name(t.source().unwrap_or(""), &mut errs);
                for e in errs {
                    self.on_error(tok_pos(t), e);
                }
                name
            }
        };
        if token.ty() == "block-seq" {
            let last_prop = match (props.anchor, tag_token) {
                (Some(a), Some(t)) => Some(if a.offset() > t.offset() { a } else { t }),
                (a, t) => a.or(t),
            };
            if let Some(lp) = last_prop
                && props.newline_after_prop.is_none_or(|nl| nl.offset() < lp.offset())
            {
                self.on_error(tok_pos(lp), "Missing newline after block sequence props");
            }
        }
        let exp_type = match token {
            Token::BlockMap { .. } => "map",
            Token::BlockSeq { .. } => "seq",
            Token::Flow { start, .. } if start.source == "{" => "map",
            _ => "seq",
        };
        let tn = tag_name.as_deref();
        if tag_token.is_none()
            || tn.is_none()
            || tn == Some("!")
            || (tn == Some(MAP_TAG) && exp_type == "map")
            || (tn == Some(SEQ_TAG) && exp_type == "seq")
        {
            let id = self.resolve_collection_node(token, None);
            if let Some(name) = tag_name {
                self.nodes[id].tag = Some(name);
            }
            return id;
        }
        let tn = tn.unwrap();
        let known = known_collection_tag(tn);
        let kt = match known {
            Some((kt, coll)) if coll == exp_type => kt,
            _ => {
                // BAD_COLLECTION_TYPE / Unresolved tag warnings
                let id = self.resolve_collection_node(token, None);
                self.nodes[id].tag = Some(tn.to_string());
                return id;
            }
        };
        let id = self.resolve_collection_node(token, Some(kt));
        let tag_tok = tag_token.unwrap();
        match kt {
            KnownTag::Set => {
                if !self.has_all_null_values(id) {
                    self.on_error(tok_pos(tag_tok), "Set items must all have null values");
                }
            }
            KnownTag::Pairs | KnownTag::OMap => {
                self.resolve_pairs(id, tag_tok);
                if kt == KnownTag::OMap {
                    let keys: Vec<NodeId> = match &self.nodes[id].kind {
                        Kind::Seq { items, .. } => items
                            .iter()
                            .map(|it| match it {
                                SeqItem::Pair(p) => p.key,
                                SeqItem::Node(n) => *n,
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    let mut seen: Vec<SVal> = Vec::new();
                    for key in keys {
                        if let Kind::Scalar { value, .. } = &self.nodes[key].kind {
                            let value = value.clone();
                            if seen.iter().any(|s| same_value_zero(s, &value)) {
                                let msg = format!(
                                    "Ordered maps must not include duplicate keys: {}",
                                    sval_to_string(&value)
                                );
                                self.on_error(tok_pos(tag_tok), msg);
                            } else {
                                seen.push(value);
                            }
                        }
                    }
                    if let Kind::Seq { class, .. } = &mut self.nodes[id].kind {
                        *class = SeqClass::OMap;
                    }
                }
            }
        }
        self.nodes[id].tag = Some(tn.to_string());
        id
    }

    fn has_all_null_values(&self, id: NodeId) -> bool {
        let Kind::Map { items, .. } = &self.nodes[id].kind else { return false };
        items.iter().all(|p| match p.value {
            None => true,
            Some(v) => {
                let n = &self.nodes[v];
                matches!(n.kind, Kind::Scalar { value: SVal::Null, .. }) && !n.comment && n.tag.is_none()
            }
        })
    }

    /// `resolvePairs(seq, onError)`.
    fn resolve_pairs(&mut self, id: NodeId, tag_tok: &Token) {
        let items = match &self.nodes[id].kind {
            Kind::Seq { items, .. } => items.clone(),
            _ => {
                self.on_error(tok_pos(tag_tok), "Expected a sequence for this tag");
                return;
            }
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match item {
                SeqItem::Pair(p) => out.push(SeqItem::Pair(p)),
                SeqItem::Node(n) => {
                    if let Kind::Map { items: map_items, .. } = &self.nodes[n].kind {
                        let map_items = map_items.clone();
                        let map_comment = self.nodes[n].comment;
                        if map_items.len() > 1 {
                            self.on_error(tok_pos(tag_tok), "Each pair must have its own sequence indicator");
                        }
                        let pair = match map_items.first() {
                            Some(p) => p.clone(),
                            None => {
                                let key = self.add_node(
                                    Kind::Scalar { value: SVal::Null, ty: None, source: String::new(), merge: false },
                                    [0, 0, 0],
                                );
                                Pair { key, value: None }
                            }
                        };
                        if map_comment {
                            let cn = pair.value.unwrap_or(pair.key);
                            self.nodes[cn].comment = true;
                        }
                        out.push(SeqItem::Pair(pair));
                    } else {
                        out.push(SeqItem::Pair(Pair { key: n, value: None }));
                    }
                }
            }
        }
        if let Kind::Seq { items, .. } = &mut self.nodes[id].kind {
            *items = out;
        }
    }

    fn resolve_collection_node(&mut self, token: &Token, tag: Option<KnownTag>) -> NodeId {
        match token {
            Token::BlockMap { .. } => self.resolve_block_map(token, tag),
            Token::BlockSeq { .. } => self.resolve_block_seq(token),
            _ => self.resolve_flow_collection(token, tag),
        }
    }

    fn map_includes(&self, items: &[Pair], search: NodeId) -> bool {
        items.iter().any(|pair| {
            if pair.key == search {
                return true;
            }
            match (&self.nodes[pair.key].kind, &self.nodes[search].kind) {
                (Kind::Scalar { value: a, .. }, Kind::Scalar { value: b, .. }) => a.strict_eq(b),
                _ => false,
            }
        })
    }

    fn map_items(&self, id: NodeId) -> &[Pair] {
        match &self.nodes[id].kind {
            Kind::Map { items, .. } => items,
            _ => &[],
        }
    }

    fn push_pair(&mut self, id: NodeId, pair: Pair) {
        if let Kind::Map { items, .. } = &mut self.nodes[id].kind {
            items.push(pair);
        }
    }

    fn resolve_block_map(&mut self, bm: &Token, tag: Option<KnownTag>) -> NodeId {
        const START_COL_MSG: &str = "All mapping items must start at the same column";
        let Token::BlockMap { offset: bm_offset, indent: bm_indent, items } = bm else { unreachable!() };
        let class = if tag == Some(KnownTag::Set) { MapClass::Set } else { MapClass::Map };
        let map = self.add_node(Kind::Map { items: Vec::new(), class }, [0, 0, 0]);
        if self.at_root {
            self.at_root = false;
        }
        let mut offset = *bm_offset;
        let mut comment_end: Option<i64> = None;
        for coll_item in items {
            let Item { start, key, sep, value, .. } = coll_item;
            let key = key.as_deref();
            let value = value.as_deref();
            // key properties
            let next = key.or_else(|| sep.as_ref().and_then(|s| s.first()));
            let key_props = self.resolve_props(start, None, "explicit-key-ind", next, offset, *bm_indent, true);
            let implicit_key = key_props.found.is_none();
            if implicit_key {
                if let Some(key) = key {
                    if key.ty() == "block-seq" {
                        self.on_error(num_pos(offset), "A block sequence may not be used as an implicit map key");
                    } else if key.indent().is_some_and(|i| i != *bm_indent) {
                        self.on_error(num_pos(offset), START_COL_MSG);
                    }
                }
                if key_props.anchor.is_none() && key_props.tag.is_none() && sep.is_none() {
                    comment_end = Some(key_props.end);
                    continue;
                }
                if key_props.newline_after_prop.is_some() || contains_newline(key) {
                    let pos = match key {
                        Some(k) => tok_pos(k),
                        None => start.last().map(tok_pos).unwrap_or((0, 1)),
                    };
                    self.on_error(pos, "Implicit keys need to be on a single line");
                }
            } else if key_props.found.and_then(|f| f.indent()) != Some(*bm_indent) {
                self.on_error(num_pos(offset), START_COL_MSG);
            }
            // key value
            self.at_key = true;
            let key_start = key_props.end;
            let key_props_start = key_props.start;
            let key_node = match key {
                Some(k) => self.compose_node(k, key_props),
                None => self.compose_empty_node(key_start, Some(start), key_props),
            };
            self.at_key = false;
            if self.map_includes(self.map_items(map), key_node) {
                self.on_error(num_pos(key_start), "Map keys must be unique");
            }
            // value properties
            let key_range = self.nodes[key_node].range;
            let value_props = self.resolve_props(
                sep.as_deref().unwrap_or(&[]),
                None,
                "map-value-ind",
                value,
                key_range[2],
                *bm_indent,
                key.is_none_or(|k| k.ty() == "block-scalar"),
            );
            offset = value_props.end;
            if let Some(found) = value_props.found {
                if implicit_key {
                    if value.is_some_and(|v| v.ty() == "block-map") && !value_props.has_newline {
                        self.on_error(num_pos(offset), "Nested mappings are not allowed in compact mappings");
                    }
                    if key_props_start < found.offset() - 1024 {
                        self.on_error(
                            range_pos(key_range),
                            "The : indicator must be at most 1024 chars after the start of an implicit block mapping key",
                        );
                    }
                }
                // value value
                let value_node = match value {
                    Some(v) => self.compose_node(v, value_props),
                    None => self.compose_empty_node(offset, sep.as_deref(), value_props),
                };
                offset = self.nodes[value_node].range[2];
                self.push_pair(map, Pair { key: key_node, value: Some(value_node) });
            } else {
                // key with no value
                if implicit_key {
                    self.on_error(range_pos(key_range), "Implicit map keys need to be followed by map values");
                }
                if !value_props.comment.is_empty() {
                    self.nodes[key_node].comment = true;
                }
                self.push_pair(map, Pair { key: key_node, value: None });
            }
        }
        if let Some(ce) = comment_end
            && ce != 0
            && ce < offset
        {
            self.on_error(num_pos(ce), "Map comment with trailing content");
        }
        self.nodes[map].range = [*bm_offset, offset, comment_end.unwrap_or(offset)];
        map
    }

    fn resolve_block_seq(&mut self, bs: &Token) -> NodeId {
        let Token::BlockSeq { offset: bs_offset, indent: bs_indent, items } = bs else { unreachable!() };
        let seq = self.add_node(Kind::Seq { items: Vec::new(), class: SeqClass::Seq }, [0, 0, 0]);
        if self.at_root {
            self.at_root = false;
        }
        if self.at_key {
            self.at_key = false;
        }
        let mut offset = *bs_offset;
        let mut comment_end: Option<i64> = None;
        for Item { start, value, .. } in items {
            let value = value.as_deref();
            let props = self.resolve_props(start, None, "seq-item-ind", value, offset, *bs_indent, true);
            if props.found.is_none() {
                if props.anchor.is_some() || props.tag.is_some() || value.is_some() {
                    if value.is_some_and(|v| v.ty() == "block-seq") {
                        self.on_error(num_pos(props.end), "All sequence items must start at the same column");
                    } else {
                        self.on_error(num_pos(offset), "Sequence item without - indicator");
                    }
                } else {
                    comment_end = Some(props.end);
                    continue;
                }
            }
            let node = match value {
                Some(v) => self.compose_node(v, props),
                None => {
                    let pe = props.end;
                    self.compose_empty_node(pe, Some(start), props)
                }
            };
            offset = self.nodes[node].range[2];
            if let Kind::Seq { items, .. } = &mut self.nodes[seq].kind {
                items.push(SeqItem::Node(node));
            }
        }
        self.nodes[seq].range = [*bs_offset, offset, comment_end.unwrap_or(offset)];
        seq
    }

    fn resolve_flow_collection(&mut self, fc: &Token, tag: Option<KnownTag>) -> NodeId {
        const BLOCK_MSG: &str = "Block collections are not allowed within flow collections";
        let is_block = |t: Option<&Token>| matches!(t.map(|t| t.ty()), Some("block-map" | "block-seq"));
        let Token::Flow { offset: fc_offset, indent: fc_indent, start: fc_start, items, end: fc_end } = fc
        else {
            unreachable!()
        };
        let is_map = fc_start.source == "{";
        let fc_name = if is_map { "flow map" } else { "flow sequence" };
        let coll = if is_map {
            let class = if tag == Some(KnownTag::Set) { MapClass::Set } else { MapClass::Map };
            self.add_node(Kind::Map { items: Vec::new(), class }, [0, 0, 0])
        } else {
            let class = if tag == Some(KnownTag::OMap) { SeqClass::OMap } else { SeqClass::Seq };
            self.add_node(Kind::Seq { items: Vec::new(), class }, [0, 0, 0])
        };
        let at_root = self.at_root;
        if at_root {
            self.at_root = false;
        }
        if self.at_key {
            self.at_key = false;
        }
        let mut offset = fc_offset + ulen(&fc_start.source);
        for (i, coll_item) in items.iter().enumerate() {
            let Item { start, key, sep, value, .. } = coll_item;
            let key = key.as_deref();
            let value = value.as_deref();
            let next = key.or_else(|| sep.as_ref().and_then(|s| s.first()));
            let mut props =
                self.resolve_props(start, Some(fc_name), "explicit-key-ind", next, offset, *fc_indent, false);
            if props.found.is_none() {
                if props.anchor.is_none() && props.tag.is_none() && sep.is_none() && value.is_none() {
                    if let Some(comma) = props.comma.filter(|_| i == 0) {
                        let msg = format!("Unexpected , in {fc_name}");
                        self.on_error(tok_pos(comma), msg);
                    } else if i < items.len() - 1 {
                        let msg = format!("Unexpected empty item in {fc_name}");
                        self.on_error(num_pos(props.start), msg);
                    }
                    offset = props.end;
                    continue;
                }
                if !is_map && contains_newline(key) {
                    self.on_error(
                        tok_pos(key.unwrap()), // checked by containsNewline()
                        "Implicit keys of flow sequence pairs need to be on a single line",
                    );
                }
            }
            if i == 0 {
                if let Some(c) = props.comma {
                    let msg = format!("Unexpected , in {fc_name}");
                    self.on_error(tok_pos(c), msg);
                }
            } else {
                if props.comma.is_none() {
                    let msg = format!("Missing , between {fc_name} items");
                    self.on_error(num_pos(props.start), msg);
                }
                if !props.comment.is_empty() {
                    let mut prev_item_comment = String::new();
                    for st in start {
                        match st.ty() {
                            "comma" | "space" => {}
                            "comment" => {
                                prev_item_comment = comment_tail(st.source().unwrap_or(""));
                                break;
                            }
                            _ => break,
                        }
                    }
                    if !prev_item_comment.is_empty() {
                        let prev = match &self.nodes[coll].kind {
                            Kind::Map { items, .. } => items.last().map(|p| p.value.unwrap_or(p.key)),
                            Kind::Seq { items, .. } => items.last().map(|it| match it {
                                SeqItem::Node(n) => *n,
                                SeqItem::Pair(p) => p.value.unwrap_or(p.key),
                            }),
                            _ => None,
                        };
                        if let Some(prev) = prev {
                            self.nodes[prev].comment = true;
                        }
                        let units: Vec<u16> = props.comment.encode_utf16().collect();
                        let skip = ulen(&prev_item_comment) as usize + 1;
                        props.comment = String::from_utf16_lossy(units.get(skip..).unwrap_or(&[]));
                    }
                }
            }
            if !is_map && sep.is_none() && props.found.is_none() {
                // item is a value in a seq
                // → key & sep are empty, start does not include ? or :
                let value_node = match value {
                    Some(v) => self.compose_node(v, props),
                    None => {
                        let pe = props.end;
                        self.compose_empty_node(pe, sep.as_deref(), props)
                    }
                };
                if let Kind::Seq { items, .. } = &mut self.nodes[coll].kind {
                    items.push(SeqItem::Node(value_node));
                }
                let range = self.nodes[value_node].range;
                offset = range[2];
                if is_block(value) {
                    self.on_error(range_pos(range), BLOCK_MSG);
                }
            } else {
                // item is a key+value pair
                // key value
                self.at_key = true;
                let key_start = props.end;
                let props_start = props.start;
                let props_found = props.found.is_some();
                let key_node = match key {
                    Some(k) => self.compose_node(k, props),
                    None => self.compose_empty_node(key_start, Some(start), props),
                };
                let key_range = self.nodes[key_node].range;
                if is_block(key) {
                    self.on_error(range_pos(key_range), BLOCK_MSG);
                }
                self.at_key = false;
                // value properties
                let value_props = self.resolve_props(
                    sep.as_deref().unwrap_or(&[]),
                    Some(fc_name),
                    "map-value-ind",
                    value,
                    key_range[2],
                    *fc_indent,
                    false,
                );
                if let Some(found) = value_props.found {
                    if !is_map && !props_found {
                        if let Some(sep) = sep {
                            for st in sep {
                                if std::ptr::eq(st, found) {
                                    break;
                                }
                                if st.ty() == "newline" {
                                    self.on_error(
                                        tok_pos(st),
                                        "Implicit keys of flow sequence pairs need to be on a single line",
                                    );
                                    break;
                                }
                            }
                        }
                        if props_start < found.offset() - 1024 {
                            self.on_error(
                                tok_pos(found),
                                "The : indicator must be at most 1024 chars after the start of an implicit flow sequence key",
                            );
                        }
                    }
                } else if let Some(v) = value {
                    if v.source().is_some_and(|s| s.starts_with(':')) {
                        let msg = format!("Missing space after : in {fc_name}");
                        self.on_error(tok_pos(v), msg);
                    } else {
                        let msg = format!("Missing , or : between {fc_name} items");
                        self.on_error(num_pos(value_props.start), msg);
                    }
                }
                // value value
                let vp_end = value_props.end;
                let vp_comment = !value_props.comment.is_empty();
                let value_node = match value {
                    Some(v) => Some(self.compose_node(v, value_props)),
                    None if value_props.found.is_some() => {
                        Some(self.compose_empty_node(vp_end, sep.as_deref(), value_props))
                    }
                    None => None,
                };
                if let Some(vn) = value_node {
                    if is_block(value) {
                        let r = self.nodes[vn].range;
                        self.on_error(range_pos(r), BLOCK_MSG);
                    }
                } else if vp_comment {
                    self.nodes[key_node].comment = true;
                }
                let pair = Pair { key: key_node, value: value_node };
                if is_map {
                    if self.map_includes(self.map_items(coll), key_node) {
                        self.on_error(num_pos(key_start), "Map keys must be unique");
                    }
                    self.push_pair(coll, pair);
                } else {
                    let end_range = self.nodes[value_node.unwrap_or(key_node)].range;
                    let map = self.add_node(
                        Kind::Map { items: vec![pair], class: MapClass::Map },
                        [key_range[0], end_range[1], end_range[2]],
                    );
                    if let Kind::Seq { items, .. } = &mut self.nodes[coll].kind {
                        items.push(SeqItem::Node(map));
                    }
                }
                offset = match value_node {
                    Some(vn) => self.nodes[vn].range[2],
                    None => vp_end,
                };
            }
        }
        let expected_end = if is_map { "}" } else { "]" };
        let mut ee: Vec<&Token> = fc_end.iter().collect();
        let ce = if ee.is_empty() { None } else { Some(ee.remove(0)) };
        let mut ce_pos = offset;
        if ce.and_then(|c| c.source()) == Some(expected_end) {
            let c = ce.unwrap();
            ce_pos = c.offset() + ulen(c.source().unwrap());
        } else {
            let name = if is_map { "Flow map" } else { "Flow sequence" };
            let msg = if at_root {
                format!("{name} must end with a {expected_end}")
            } else {
                format!("{name} in block collection must be sufficiently indented and end with a {expected_end}")
            };
            self.on_error(num_pos(offset), msg);
            if let Some(c) = ce
                && c.source().map(ulen) != Some(1)
            {
                ee.insert(0, c);
            }
        }
        if !ee.is_empty() {
            let owned: Vec<Token> = ee.into_iter().cloned().collect();
            let (comment, end_offset) = self.resolve_end(Some(&owned), ce_pos, true);
            if !comment.is_empty() {
                self.nodes[coll].comment = true;
            }
            self.nodes[coll].range = [*fc_offset, ce_pos, end_offset];
        } else {
            self.nodes[coll].range = [*fc_offset, ce_pos, ce_pos];
        }
        coll
    }

    fn compose_scalar(&mut self, token: &Token, tag_token: Option<&Token>) -> NodeId {
        let (value, ty, comment, range) = match token {
            Token::BlockScalar { .. } => self.resolve_block_scalar(token),
            Token::Src(st) => self.resolve_flow_scalar(st),
            _ => unreachable!(),
        };
        let tag_name = match tag_token {
            None => None,
            Some(t) => {
                let mut errs = Vec::new();
                let name = self.doc_directives.tag_name(t.source().unwrap_or(""), &mut errs);
                for e in errs {
                    self.on_error(tok_pos(t), e);
                }
                name
            }
        };
        let tag = if let Some(name) = tag_name.as_deref() {
            find_scalar_tag_by_name(&value, name)
        } else if token.ty() == "scalar" {
            find_scalar_tag_by_test(&value)
        } else {
            ScalarTag::Str
        };
        let err_pos = tok_pos(tag_token.unwrap_or(token));
        let mut merge = false;
        let resolved = match tag.resolve(&value) {
            Ok(v) => {
                merge = tag == ScalarTag::Merge;
                v
            }
            Err(msg) => {
                self.on_error(err_pos, msg);
                SVal::Str(value.clone())
            }
        };
        let id = self.add_node(Kind::Scalar { value: resolved, ty, source: value, merge }, range);
        if let Some(name) = tag_name {
            self.nodes[id].tag = Some(name);
        }
        if comment {
            self.nodes[id].comment = true;
        }
        id
    }

    /// `resolveFlowScalar(scalar, strict, onError)` → `(value, type, has comment, range)`.
    fn resolve_flow_scalar(&mut self, st: &St) -> (String, Option<ScalarType>, bool, [i64; 3]) {
        let offset = st.offset;
        let units: Vec<u16> = st.source.encode_utf16().collect();
        let mut rel_errors: Vec<(i64, String)> = Vec::new();
        let (ty, value) = match st.ty {
            "scalar" => (ScalarType::Plain, plain_value(&st.source, &mut rel_errors)),
            "single-quoted-scalar" => (ScalarType::QuoteSingle, single_quoted_value(&units, &mut rel_errors)),
            "double-quoted-scalar" => (ScalarType::QuoteDouble, double_quoted_value(&units, &mut rel_errors)),
            other => {
                let msg = format!("Expected a flow scalar value, but found: {other}");
                self.on_error(tok_pos(&Token::Src(st.clone())), msg);
                let end = offset + units.len() as i64;
                return (String::new(), None, false, [offset, end, end]);
            }
        };
        for (rel, msg) in rel_errors {
            self.on_error(num_pos(offset + rel), msg);
        }
        let value_end = offset + units.len() as i64;
        let (comment, re_offset) = self.resolve_end(st.end.as_deref(), value_end, true);
        (value, Some(ty), !comment.is_empty(), [offset, value_end, re_offset])
    }

    fn resolve_block_scalar(&mut self, token: &Token) -> (String, Option<ScalarType>, bool, [i64; 3]) {
        let Token::BlockScalar { offset, indent: scalar_indent, props, source } = token else { unreachable!() };
        let start = *offset;
        let Some(header) = self.parse_block_scalar_header(*offset, props) else {
            return (String::new(), None, false, [start, start, start]);
        };
        let ty = if header.mode == '>' { ScalarType::BlockFolded } else { ScalarType::BlockLiteral };
        let lines = if source.is_empty() { Vec::new() } else { split_lines(source) };
        // determine the end of content & start of chomping
        let mut chomp_start = lines.len();
        for i in (0..lines.len()).rev() {
            let content = &lines[i].1;
            if content.is_empty() || content == "\r" {
                chomp_start = i;
            } else {
                break;
            }
        }
        let has_comment = !header.comment.is_empty();
        // shortcut for empty contents
        if chomp_start == 0 {
            let value = if header.chomp == '+' && !lines.is_empty() {
                "\n".repeat(1.max(lines.len() as i64 - 1) as usize)
            } else {
                String::new()
            };
            let mut end = start + header.length;
            if !source.is_empty() {
                end += ulen(source);
            }
            return (value, Some(ty), has_comment, [start, end, end]);
        }
        // find the indentation level to trim from start
        let mut trim_indent = scalar_indent + header.indent;
        let mut offset = offset + header.length;
        let mut content_start = 0;
        for (i, (indent, content)) in lines.iter().enumerate().take(chomp_start) {
            let indent_len = indent.len() as i64;
            if content.is_empty() || content == "\r" {
                if header.indent == 0 && indent_len > trim_indent {
                    trim_indent = indent_len;
                }
            } else {
                if indent_len < trim_indent {
                    let message = "Block scalars with more-indented leading empty lines must use an explicit indentation indicator";
                    self.on_error(num_pos(offset + indent_len), message);
                }
                if header.indent == 0 {
                    trim_indent = indent_len;
                }
                content_start = i;
                if trim_indent == 0 && !self.at_root {
                    let message = "Block scalar values in collections must be indented";
                    self.on_error(num_pos(offset), message);
                }
                break;
            }
            offset += indent_len + ulen(content) + 1;
        }
        // include trailing more-indented empty lines in content
        for i in (chomp_start..lines.len()).rev() {
            if lines[i].0.len() as i64 > trim_indent {
                chomp_start = i + 1;
            }
        }
        let slice_indent = |indent: &str| -> String {
            let t = trim_indent.max(0) as usize;
            if t >= indent.len() { String::new() } else { indent[t..].to_string() }
        };
        let mut value = String::new();
        let mut sep = "";
        let mut prev_more_indented = false;
        // leading whitespace is kept intact
        for line in lines.iter().take(content_start) {
            value.push_str(&slice_indent(&line.0));
            value.push('\n');
        }
        for line in lines.iter().take(chomp_start).skip(content_start) {
            let (mut indent, mut content) = (line.0.clone(), line.1.clone());
            offset += indent.len() as i64 + ulen(&content) + 1;
            let crlf = content.ends_with('\r');
            if crlf {
                content.pop();
            }
            if !content.is_empty() && (indent.len() as i64) < trim_indent {
                let src = if header.indent != 0 { "explicit indentation indicator" } else { "first line" };
                let message = format!("Block scalar lines must not be less indented than their {src}");
                self.on_error(num_pos(offset - ulen(&content) - if crlf { 2 } else { 1 }), message);
                indent = String::new();
            }
            if ty == ScalarType::BlockLiteral {
                value.push_str(sep);
                value.push_str(&slice_indent(&indent));
                value.push_str(&content);
                sep = "\n";
            } else if indent.len() as i64 > trim_indent || content.starts_with('\t') {
                // more-indented content within a folded block
                if sep == " " {
                    sep = "\n";
                } else if !prev_more_indented && sep == "\n" {
                    sep = "\n\n";
                }
                value.push_str(sep);
                value.push_str(&slice_indent(&indent));
                value.push_str(&content);
                sep = "\n";
                prev_more_indented = true;
            } else if content.is_empty() {
                // empty line
                if sep == "\n" {
                    value.push('\n');
                } else {
                    sep = "\n";
                }
            } else {
                value.push_str(sep);
                value.push_str(&content);
                sep = " ";
                prev_more_indented = false;
            }
        }
        match header.chomp {
            '-' => {}
            '+' => {
                for line in lines.iter().skip(chomp_start) {
                    value.push('\n');
                    value.push_str(&slice_indent(&line.0));
                }
                if !value.ends_with('\n') {
                    value.push('\n');
                }
            }
            _ => value.push('\n'),
        }
        let end = start + header.length + ulen(source);
        (value, Some(ty), has_comment, [start, end, end])
    }

    fn parse_block_scalar_header(&mut self, offset: i64, props: &[Token]) -> Option<BlockHeader> {
        if props.first().map(|p| p.ty()) != Some("block-scalar-header") {
            if let Some(p) = props.first() {
                self.on_error(tok_pos(p), "Block scalar header not found");
            }
            return None;
        }
        let source = props[0].source().unwrap_or("");
        let units: Vec<u16> = source.encode_utf16().collect();
        let mode = source.chars().next().unwrap_or('|');
        let mut indent = 0i64;
        let mut chomp = '\0';
        let mut error = -1i64;
        for (i, &u) in units.iter().enumerate().skip(1) {
            let ch = char::from_u32(u as u32);
            if chomp == '\0' && (ch == Some('-') || ch == Some('+')) {
                chomp = ch.unwrap();
            } else {
                let n = match ch {
                    Some(c) if c.is_ascii_digit() => c as i64 - '0' as i64,
                    _ => 0,
                };
                if indent == 0 && n != 0 {
                    indent = n;
                } else if error == -1 {
                    error = offset + i as i64;
                }
            }
        }
        if error != -1 {
            let msg = format!("Block scalar header includes extra characters: {source}");
            self.on_error(num_pos(error), msg);
        }
        let mut has_space = false;
        let mut comment = String::new();
        let mut length = units.len() as i64;
        for token in &props[1..] {
            match token.ty() {
                "space" | "newline" => {
                    if token.ty() == "space" {
                        has_space = true;
                    }
                    length += ulen(token.source().unwrap_or(""));
                }
                "comment" => {
                    if !has_space {
                        let message = "Comments must be separated from other tokens by white space characters";
                        self.on_error(tok_pos(token), message);
                    }
                    let s = token.source().unwrap_or("");
                    length += ulen(s);
                    comment = comment_tail(s);
                }
                "error" => {
                    let Token::Error { message, .. } = token else { unreachable!() };
                    let message = message.clone();
                    self.on_error(tok_pos(token), message);
                    length += ulen(token.source().unwrap_or(""));
                }
                ty => {
                    let message = format!("Unexpected token in block scalar header: {ty}");
                    self.on_error(tok_pos(token), message);
                    if let Some(ts) = token.source()
                        && !ts.is_empty()
                    {
                        length += ulen(ts);
                    }
                }
            }
        }
        Some(BlockHeader { mode, indent, chomp, comment, length })
    }
}

struct BlockHeader {
    mode: char,
    indent: i64,
    chomp: char,
    comment: String,
    length: i64,
}

fn anchor_name(anchor: &Token) -> String {
    let units: Vec<u16> = anchor.source().unwrap_or("").encode_utf16().collect();
    String::from_utf16_lossy(units.get(1..).unwrap_or(&[]))
}

/// `source.substring(1) || ' '`.
fn comment_body(source: &str) -> String {
    let tail = comment_tail(source);
    if tail.is_empty() { " ".to_string() } else { tail }
}

/// `source.substring(1)`.
fn comment_tail(source: &str) -> String {
    let units: Vec<u16> = source.encode_utf16().collect();
    String::from_utf16_lossy(units.get(1..).unwrap_or(&[]))
}

/// `splitLines(source)`: `[indent, content]` pairs (`source.split(/\n( *)/)`).
fn split_lines(source: &str) -> Vec<(String, String)> {
    let mut lines = Vec::new();
    let mut parts = source.split('\n');
    let first = parts.next().unwrap_or("");
    let n = first.len() - first.trim_start_matches(' ').len();
    lines.push((first[..n].to_string(), first[n..].to_string()));
    for part in parts {
        let n = part.len() - part.trim_start_matches(' ').len();
        lines.push((part[..n].to_string(), part[n..].to_string()));
    }
    lines
}

/// `emptyScalarPosition(offset, before, pos)` with `pos` = `before.length`.
fn empty_scalar_position(mut offset: i64, before: Option<&[Token]>) -> i64 {
    if let Some(before) = before {
        let mut i = before.len() as i64 - 1;
        while i >= 0 {
            let st = &before[i as usize];
            match st.ty() {
                "space" | "comment" | "newline" => {
                    offset -= ulen(st.source().unwrap_or(""));
                    i -= 1;
                    continue;
                }
                _ => {}
            }
            // Technically, an empty scalar is immediately after the last non-empty
            // node, but it's more useful to place it after any whitespace.
            i += 1;
            while let Some(st) = before.get(i as usize) {
                if st.ty() != "space" {
                    break;
                }
                offset += ulen(st.source().unwrap_or(""));
                i += 1;
            }
            break;
        }
    }
    offset
}

/// `containsNewline(key)`.
fn contains_newline(key: Option<&Token>) -> bool {
    let Some(key) = key else { return false };
    match key {
        Token::Src(st) if matches!(st.ty, "alias" | "scalar" | "double-quoted-scalar" | "single-quoted-scalar") => {
            if st.source.contains('\n') {
                return true;
            }
            if let Some(end) = &st.end {
                for t in end {
                    if t.ty() == "newline" {
                        return true;
                    }
                }
            }
            false
        }
        Token::Flow { items, .. } => {
            for it in items {
                if it.start.iter().any(|t| t.ty() == "newline") {
                    return true;
                }
                if let Some(sep) = &it.sep
                    && sep.iter().any(|t| t.ty() == "newline")
                {
                    return true;
                }
                if contains_newline(it.key.as_deref()) || contains_newline(it.value.as_deref()) {
                    return true;
                }
            }
            false
        }
        _ => true,
    }
}

fn plain_value(source: &str, errors: &mut Vec<(i64, String)>) -> String {
    let bad_char = match source.chars().next() {
        Some('\t') => "a tab character".to_string(),
        Some(',') => "flow indicator character ,".to_string(),
        Some('%') => "directive indicator character %".to_string(),
        Some(c @ ('|' | '>')) => format!("block scalar indicator {c}"),
        Some(c @ ('@' | '`')) => format!("reserved character {c}"),
        _ => String::new(),
    };
    if !bad_char.is_empty() {
        errors.push((0, format!("Plain value cannot start with {bad_char}")));
    }
    fold_lines(source)
}

fn single_quoted_value(units: &[u16], errors: &mut Vec<(i64, String)>) -> String {
    if units.last() != Some(&(b'\'' as u16)) || units.len() == 1 {
        errors.push((units.len() as i64, "Missing closing 'quote".to_string()));
    }
    let inner = if units.len() >= 2 { &units[1..units.len() - 1] } else { &[][..] };
    fold_lines(&String::from_utf16_lossy(inner)).replace("''", "'")
}

/// `foldLines(source)`.
fn fold_lines(source: &str) -> String {
    // first = /(.*?)(?<![ \t])[ \t]*\r?\n/sy
    let Some(nl) = source.find('\n') else { return source.to_string() };
    let trim_line = |line: &str| -> String {
        let line = line.strip_suffix('\r').unwrap_or(line);
        line.trim_end_matches([' ', '\t']).to_string()
    };
    let mut res = trim_line(&source[..nl]);
    let mut sep = " ";
    let mut pos = nl + 1;
    // line = /[ \t]*(.*?)(?:(?<![ \t])[ \t]*)?\r?\n/sy
    while let Some(rel) = source[pos..].find('\n') {
        let line = &source[pos..pos + rel];
        let content = trim_line(line.trim_start_matches([' ', '\t']));
        // A line that is only "\r" keeps it: `.*?` stops before `\r?\n`.
        if content.is_empty() {
            if sep == "\n" {
                res.push_str(sep);
            } else {
                sep = "\n";
            }
        } else {
            res.push_str(sep);
            res.push_str(&content);
            sep = " ";
        }
        pos += rel + 1;
    }
    // last = /[ \t]*(.*)/sy
    let last = source[pos..].trim_start_matches([' ', '\t']);
    format!("{res}{sep}{last}")
}

fn double_quoted_value(source: &[u16], errors: &mut Vec<(i64, String)>) -> String {
    let at = |i: usize| -> Option<u16> { source.get(i).copied() };
    let is = |c: Option<u16>, ch: char| c == Some(ch as u16);
    let mut res: Vec<u16> = Vec::new();
    let mut i = 1usize;
    while i + 1 < source.len() {
        let ch = source[i];
        if ch == b'\r' as u16 && is(at(i + 1), '\n') {
            i += 1;
            continue;
        }
        if ch == b'\n' as u16 {
            let (fold, offset) = fold_newline(source, i);
            res.extend(fold.encode_utf16());
            i = offset;
        } else if ch == b'\\' as u16 {
            i += 1;
            let mut next = at(i);
            let cc = next.and_then(escape_code);
            if let Some(cc) = cc {
                res.extend(cc.encode_utf16());
            } else if is(next, '\n') {
                // skip escaped newlines, but still trim the following line
                next = at(i + 1);
                while is(next, ' ') || is(next, '\t') {
                    i += 1;
                    next = at(i + 1);
                }
            } else if is(next, '\r') && is(at(i + 1), '\n') {
                // skip escaped CRLF newlines, but still trim the following line
                i += 1;
                next = at(i + 1);
                while is(next, ' ') || is(next, '\t') {
                    i += 1;
                    next = at(i + 1);
                }
            } else if is(next, 'x') || is(next, 'u') || is(next, 'U') {
                let length = if is(next, 'x') {
                    2
                } else if is(next, 'u') {
                    4
                } else {
                    8
                };
                res.extend(parse_char_code(source, i + 1, length, errors));
                i += length;
            } else {
                let raw_end = (i + 1).min(source.len());
                let raw = &source[i - 1..raw_end];
                let raw_s = String::from_utf16_lossy(raw);
                errors.push((i as i64 - 1, format!("Invalid escape sequence {raw_s}")));
                res.extend_from_slice(raw);
            }
        } else if ch == b' ' as u16 || ch == b'\t' as u16 {
            // trim trailing whitespace
            let ws_start = i;
            let mut next = at(i + 1);
            while is(next, ' ') || is(next, '\t') {
                i += 1;
                next = at(i + 1);
            }
            if !is(next, '\n') && !(is(next, '\r') && is(at(i + 2), '\n')) {
                if i > ws_start {
                    res.extend_from_slice(&source[ws_start..i + 1]);
                } else {
                    res.push(ch);
                }
            }
        } else {
            res.push(ch);
        }
        i += 1;
    }
    if source.last() != Some(&(b'"' as u16)) || source.len() == 1 {
        errors.push((source.len() as i64, "Missing closing \"quote".to_string()));
    }
    String::from_utf16_lossy(&res)
}

fn fold_newline(source: &[u16], mut offset: usize) -> (String, usize) {
    let mut fold = String::new();
    let mut ch = source.get(offset + 1).copied();
    while matches!(ch, Some(c) if c == b' ' as u16 || c == b'\t' as u16 || c == b'\n' as u16 || c == b'\r' as u16)
    {
        if ch == Some(b'\r' as u16) && source.get(offset + 2).copied() != Some(b'\n' as u16) {
            break;
        }
        if ch == Some(b'\n' as u16) {
            fold.push('\n');
        }
        offset += 1;
        ch = source.get(offset + 1).copied();
    }
    if fold.is_empty() {
        fold = " ".to_string();
    }
    (fold, offset)
}

fn escape_code(c: u16) -> Option<&'static str> {
    Some(match char::from_u32(c as u32)? {
        '0' => "\0",
        'a' => "\x07",
        'b' => "\x08",
        'e' => "\x1b",
        'f' => "\x0c",
        'n' => "\n",
        'r' => "\r",
        't' => "\t",
        'v' => "\x0b",
        'N' => "\u{85}",
        '_' => "\u{a0}",
        'L' => "\u{2028}",
        'P' => "\u{2029}",
        ' ' => " ",
        '"' => "\"",
        '/' => "/",
        '\\' => "\\",
        '\t' => "\t",
        _ => return None,
    })
}

fn parse_char_code(source: &[u16], offset: usize, length: usize, errors: &mut Vec<(i64, String)>) -> Vec<u16> {
    let end = (offset + length).min(source.len());
    let cc = source.get(offset..end).unwrap_or(&[]);
    let ok = cc.len() == length && cc.iter().all(|c| *c < 128 && (*c as u8).is_ascii_hexdigit());
    let code = if ok {
        u32::from_str_radix(&String::from_utf16_lossy(cc), 16).ok()
    } else {
        None
    };
    match code {
        Some(code) if code <= 0x10FFFF => {
            if (0xD800..=0xDFFF).contains(&code) {
                vec![code as u16]
            } else {
                let c = char::from_u32(code).unwrap();
                let mut buf = [0u16; 2];
                c.encode_utf16(&mut buf).to_vec()
            }
        }
        _ => {
            let start = offset.saturating_sub(2);
            let raw_end = (start + length + 2).min(source.len());
            let raw = &source[start..raw_end];
            let raw_s = String::from_utf16_lossy(raw);
            errors.push((offset as i64 - 2, format!("Invalid escape sequence {raw_s}")));
            raw.to_vec()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KnownTag {
    Set,
    OMap,
    Pairs,
}

fn known_collection_tag(name: &str) -> Option<(KnownTag, &'static str)> {
    match name {
        "tag:yaml.org,2002:set" => Some((KnownTag::Set, "map")),
        "tag:yaml.org,2002:omap" => Some((KnownTag::OMap, "seq")),
        "tag:yaml.org,2002:pairs" => Some((KnownTag::Pairs, "seq")),
        // binary, merge, timestamp are scalar tags: BAD_COLLECTION_TYPE (a warning)
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarTag {
    Str,
    Null,
    Bool,
    IntOct,
    Int,
    IntHex,
    FloatNaN,
    FloatExp,
    Float,
    Binary,
    Merge,
    Timestamp,
}

const TESTED_TAGS: [ScalarTag; 8] = [
    ScalarTag::Null,
    ScalarTag::Bool,
    ScalarTag::IntOct,
    ScalarTag::Int,
    ScalarTag::IntHex,
    ScalarTag::FloatNaN,
    ScalarTag::FloatExp,
    ScalarTag::Float,
];

impl ScalarTag {
    fn name(self) -> &'static str {
        match self {
            ScalarTag::Str => STR_TAG,
            ScalarTag::Null => "tag:yaml.org,2002:null",
            ScalarTag::Bool => "tag:yaml.org,2002:bool",
            ScalarTag::IntOct | ScalarTag::Int | ScalarTag::IntHex => "tag:yaml.org,2002:int",
            ScalarTag::FloatNaN | ScalarTag::FloatExp | ScalarTag::Float => "tag:yaml.org,2002:float",
            ScalarTag::Binary => "tag:yaml.org,2002:binary",
            ScalarTag::Merge => "tag:yaml.org,2002:merge",
            ScalarTag::Timestamp => "tag:yaml.org,2002:timestamp",
        }
    }

    fn test(self, v: &str) -> bool {
        let b = v.as_bytes();
        let digits = |s: &[u8]| !s.is_empty() && s.iter().all(|c| c.is_ascii_digit());
        fn signless(s: &[u8]) -> &[u8] {
            match s.first() {
                Some(b'-') | Some(b'+') => &s[1..],
                _ => s,
            }
        }
        match self {
            ScalarTag::Null => matches!(v, "" | "~" | "null" | "Null" | "NULL"),
            ScalarTag::Bool => matches!(v, "true" | "True" | "TRUE" | "false" | "False" | "FALSE"),
            ScalarTag::IntOct => {
                b.starts_with(b"0o") && b.len() > 2 && b[2..].iter().all(|c| (b'0'..=b'7').contains(c))
            }
            ScalarTag::Int => digits(signless(b)),
            ScalarTag::IntHex => b.starts_with(b"0x") && b.len() > 2 && b[2..].iter().all(|c| c.is_ascii_hexdigit()),
            ScalarTag::FloatNaN => {
                matches!(signless(b), b".inf" | b".Inf" | b".INF") || matches!(v, ".nan" | ".NaN" | ".NAN")
            }
            ScalarTag::FloatExp => {
                let s = signless(b);
                let Some(e) = s.iter().position(|c| *c == b'e' || *c == b'E') else { return false };
                let (mant, exp) = (&s[..e], &s[e + 1..]);
                float_mantissa(mant, true) && digits(signless(exp))
            }
            ScalarTag::Float => float_mantissa(signless(b), false),
            ScalarTag::Str | ScalarTag::Binary | ScalarTag::Merge | ScalarTag::Timestamp => false,
        }
    }

    fn resolve(self, v: &str) -> Result<SVal, String> {
        Ok(match self {
            ScalarTag::Str => SVal::Str(v.to_string()),
            ScalarTag::Null => SVal::Null,
            ScalarTag::Bool => SVal::Bool(v.starts_with('t') || v.starts_with('T')),
            ScalarTag::IntOct => SVal::Num(parse_int_radix(&v[2..], 8)),
            ScalarTag::Int => SVal::Num(js_parse_float(v)),
            ScalarTag::IntHex => SVal::Num(parse_int_radix(&v[2..], 16)),
            ScalarTag::FloatNaN => SVal::Num(if v.to_ascii_lowercase().ends_with("nan") {
                f64::NAN
            } else if v.starts_with('-') {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }),
            ScalarTag::FloatExp | ScalarTag::Float => SVal::Num(js_parse_float(v)),
            ScalarTag::Binary => SVal::Buffer(node_base64_decode(v)),
            ScalarTag::Merge => SVal::Symbol,
            ScalarTag::Timestamp => SVal::Date(resolve_timestamp(v)?),
        })
    }
}

/// `[0-9]+(?:\.[0-9]*)?` / `\.[0-9]+` (exp) or `\.[0-9]+` / `[0-9]+\.[0-9]*` (float).
fn float_mantissa(s: &[u8], exp: bool) -> bool {
    if let Some(rest) = s.strip_prefix(b".") {
        return !rest.is_empty() && rest.iter().all(|c| c.is_ascii_digit());
    }
    let int_len = s.iter().take_while(|c| c.is_ascii_digit()).count();
    if int_len == 0 {
        return false;
    }
    let rest = &s[int_len..];
    if rest.is_empty() {
        return exp;
    }
    rest[0] == b'.' && rest[1..].iter().all(|c| c.is_ascii_digit())
}

/// `parseFloat` for strings the core schema tests accept.
fn js_parse_float(v: &str) -> f64 {
    let s = v.strip_prefix('+').unwrap_or(v);
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let mut t = String::with_capacity(s.len() + 2);
    if s.starts_with('.') {
        t.push('0');
    }
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    t.push_str(mant);
    if t.ends_with('.') {
        t.push('0');
    }
    if let Some(exp) = exp {
        t.push('e');
        t.push_str(exp.strip_prefix('+').unwrap_or(exp));
    }
    let n: f64 = t.parse().unwrap_or(f64::NAN);
    if neg { -n } else { n }
}

/// `parseInt(digits, radix)` for radix 8 or 16 (correctly rounded like V8).
fn parse_int_radix(digits: &str, radix: u32) -> f64 {
    let mut value: u128 = 0;
    let mut overflow = false;
    for c in digits.chars() {
        let d = c.to_digit(radix).unwrap_or(0) as u128;
        match value.checked_mul(radix as u128).and_then(|v| v.checked_add(d)) {
            Some(v) => value = v,
            None => {
                overflow = true;
                break;
            }
        }
    }
    if !overflow {
        return value as f64;
    }
    digits
        .chars()
        .fold(0f64, |acc, c| acc * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64)
}

/// Node's `Buffer.from(src, 'base64')`: base64 and base64url characters, other characters
/// skipped, decoding stops at the first `=`.
fn node_base64_decode(src: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in src.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// `timestamp.resolve(str)` → epoch milliseconds (NaN for an invalid date).
fn resolve_timestamp(s: &str) -> Result<f64, String> {
    let err = || "!!timestamp expects a date, starting with yyyy-mm-dd".to_string();
    let b = s.as_bytes();
    let mut i = 0;
    let take_digits = |i: &mut usize, min: usize, max: usize| -> Option<&str> {
        let start = *i;
        while *i < b.len() && *i - start < max && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i - start < min { None } else { Some(&s[start..*i]) }
    };
    let year = take_digits(&mut i, 4, 4).ok_or_else(err)?;
    if b.get(i) != Some(&b'-') {
        return Err(err());
    }
    i += 1;
    let month = take_digits(&mut i, 1, 2).ok_or_else(err)?;
    if b.get(i) != Some(&b'-') {
        return Err(err());
    }
    i += 1;
    let day = take_digits(&mut i, 1, 2).ok_or_else(err)?;
    let num = |t: &str| t.parse::<f64>().unwrap_or(0.0);
    let (mut hour, mut minute, mut second, mut millis) = (0.0, 0.0, 0.0, 0.0);
    let mut tz: Option<&str> = None;
    if i < b.len() {
        // (?:t|T|[ \t]+)
        if b[i] == b't' || b[i] == b'T' {
            i += 1;
        } else if b[i] == b' ' || b[i] == b'\t' {
            while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
                i += 1;
            }
        } else {
            return Err(err());
        }
        hour = num(take_digits(&mut i, 1, 2).ok_or_else(err)?);
        if b.get(i) != Some(&b':') {
            return Err(err());
        }
        i += 1;
        minute = num(take_digits(&mut i, 1, 2).ok_or_else(err)?);
        if b.get(i) != Some(&b':') {
            return Err(err());
        }
        i += 1;
        let sec = take_digits(&mut i, 1, 2).ok_or_else(err)?;
        second = num(sec);
        if b.get(i) == Some(&b'.') {
            let fstart = i;
            i += 1;
            let digits_start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == digits_start {
                return Err(err());
            }
            // Number((match[7] + '00').substr(1, 3))
            let frac = format!("{}00", &s[fstart..i]);
            millis = num(&frac[1..4.min(frac.len())]);
        }
        // (?:[ \t]*(Z|[-+][012]?[0-9](?::[0-9]{2})?))?
        let save = i;
        let mut j = i;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        if j < b.len() {
            if b[j] == b'Z' {
                tz = Some("Z");
                j += 1;
            } else if b[j] == b'-' || b[j] == b'+' {
                let tz_start = j;
                j += 1;
                let d0 = j;
                while j < b.len() && j - d0 < 2 && b[j].is_ascii_digit() {
                    j += 1;
                }
                let ds = &b[d0..j];
                let valid = match ds.len() {
                    1 => true,
                    2 => matches!(ds[0], b'0' | b'1' | b'2'),
                    _ => false,
                };
                if !valid {
                    return Err(err());
                }
                if b.get(j) == Some(&b':') {
                    // (?::[0-9]{2})? followed by the end of input
                    if b.len() >= j + 3 && b[j + 1].is_ascii_digit() && b[j + 2].is_ascii_digit() {
                        j += 3;
                    } else {
                        return Err(err());
                    }
                }
                tz = Some(&s[tz_start..j]);
            } else {
                return Err(err());
            }
            i = j;
        } else {
            i = save;
            if j != save {
                // trailing whitespace only: no match
                return Err(err());
            }
        }
        if i != b.len() {
            return Err(err());
        }
    }
    let mut year = num(year);
    if (0.0..=99.0).contains(&year) {
        year += 1900.0; // Date.UTC
    }
    let mut date = date_utc(year, num(month) - 1.0, num(day), hour, minute, second, millis);
    if let Some(tz) = tz
        && tz != "Z"
    {
        let mut d = parse_sexagesimal(tz);
        if d.abs() < 30.0 {
            d *= 60.0;
        }
        date -= 60000.0 * d;
    }
    Ok(time_clip(date))
}

fn parse_sexagesimal(s: &str) -> f64 {
    let (sign, parts) = match s.as_bytes().first() {
        Some(b'-') | Some(b'+') => (&s[..1], &s[1..]),
        _ => ("", s),
    };
    let res = parts
        .replace('_', "")
        .split(':')
        .fold(0.0, |res, p| res * 60.0 + p.parse::<f64>().unwrap_or(f64::NAN));
    if sign == "-" { -res } else { res }
}

fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > 8.64e15 { f64::NAN } else { t.trunc() + 0.0 }
}

/// `Date.UTC(year, month, day, h, m, s, ms)` (year already adjusted).
fn date_utc(year: f64, month: f64, day: f64, h: f64, m: f64, s: f64, ms: f64) -> f64 {
    let ym = year + (month / 12.0).floor();
    let mn = month.rem_euclid(12.0);
    let days = days_from_civil(ym as i64, mn as i64 + 1, 1) as f64 + day - 1.0;
    days * 86_400_000.0 + h * 3_600_000.0 + m * 60_000.0 + s * 1000.0 + ms
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn find_scalar_tag_by_name(value: &str, tag_name: &str) -> ScalarTag {
    if tag_name == "!" {
        return ScalarTag::Str; // non-specific tag
    }
    if tag_name == STR_TAG {
        return ScalarTag::Str;
    }
    for tag in TESTED_TAGS {
        if tag.name() == tag_name && tag.test(value) {
            return tag;
        }
    }
    match tag_name {
        "tag:yaml.org,2002:binary" => ScalarTag::Binary,
        "tag:yaml.org,2002:merge" => ScalarTag::Merge,
        "tag:yaml.org,2002:timestamp" => ScalarTag::Timestamp,
        // 'Unresolved tag' is a warning except for !!str, which always resolves
        _ => ScalarTag::Str,
    }
}

fn find_scalar_tag_by_test(value: &str) -> ScalarTag {
    TESTED_TAGS.into_iter().find(|t| t.test(value)).unwrap_or(ScalarTag::Str)
}

/// SameValueZero on scalar values (`Array.prototype.includes`).
fn same_value_zero(a: &SVal, b: &SVal) -> bool {
    match (a, b) {
        (SVal::Num(x), SVal::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        _ => a.strict_eq(b),
    }
}

/// `String(value)` for scalar values.
pub(super) fn sval_to_string(v: &SVal) -> String {
    match v {
        SVal::Null => "null".to_string(),
        SVal::Bool(b) => b.to_string(),
        SVal::Num(n) => pi_ai::utils::js_json::number_to_string(*n),
        SVal::Str(s) => s.clone(),
        SVal::Buffer(b) => String::from_utf8_lossy(b).into_owned(),
        SVal::Date(_) => "Invalid Date".to_string(),
        SVal::Symbol => "Symbol(<<)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_lines_matches_js_regex_semantics() {
        assert_eq!(fold_lines("a b \nc"), "a b c");
        assert_eq!(fold_lines("a\n\nb"), "a\nb");
        assert_eq!(fold_lines("a\n\n\nb"), "a\n\nb");
        assert_eq!(fold_lines("  \nfoo"), " foo");
        assert_eq!(fold_lines("a\n  b  \n c "), "a b c ");
        assert_eq!(fold_lines("no newline"), "no newline");
    }

    #[test]
    fn decodes_uri_components() {
        assert_eq!(decode_uri_component("a%20b").as_deref(), Some("a b"));
        assert_eq!(decode_uri_component("%C3%A9").as_deref(), Some("é"));
        assert_eq!(decode_uri_component("%C3"), None);
        assert_eq!(decode_uri_component("%FF"), None);
    }

    #[test]
    fn timestamps_resolve_like_date_utc() {
        assert_eq!(resolve_timestamp("2001-12-14").unwrap(), 1_008_288_000_000.0);
        assert_eq!(resolve_timestamp("2001-12-14t21:59:43.10-05:00").unwrap(), 1_008_385_183_100.0);
        assert!(resolve_timestamp("2001-12").is_err());
    }
}
