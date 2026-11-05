//! A port of the highlight.js 10.7.3 core (`lib/core.js`): grammar compilation, the
//! highlighting state machine, relevance-based language detection and the HTML renderer.
//!
//! Language grammars are the highlight.js definitions, dumped to JSON object graphs (see
//! `scripts/dump-highlightjs-languages.js`) and embedded compressed. Grammars are compiled
//! the way highlight.js compiles them, on a small JS-like object heap, so in-place mutations
//! of shared mode objects (variants, `self`, `$pattern` deletion, frozen common modes) behave
//! exactly as in JavaScript. JavaScript callbacks used by grammars are implemented natively
//! ([`Callback`]).
//!
//! highlight.js is Copyright (c) 2006, Ivan Sagalaev, BSD 3-Clause License (see
//! `HIGHLIGHTJS_LICENSE`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, OnceLock};

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde_json::Value as Json;

use super::js_regex;

// ============================================================================
// JS-like object heap
// ============================================================================

/// Native implementations of the JavaScript callbacks found in grammars.
#[derive(Debug, Clone)]
pub enum Callback {
    /// `SHEBANG`'s `on:begin`: only match at the start of the text.
    Shebang,
    /// `END_SAME_AS_BEGIN` `on:begin`: remember match group 1.
    EndSameAsBeginBegin,
    /// `END_SAME_AS_BEGIN` `on:end`: require the same group 1.
    EndSameAsBeginEnd,
    /// JavaScript/TypeScript `XML_TAG.isTrulyOpeningTag`.
    JsxIsTrulyOpeningTag,
    /// Mathematica: ignore the match unless it is a known symbol.
    InSet(Arc<HashSet<String>>),
    /// Internal `skipIfhasPrecedingDot` (`beginKeywords`).
    SkipIfHasPrecedingDot,
    /// R's `beforeMatch` compiler extension.
    BeforeMatchExtension,
}

#[derive(Debug, Clone)]
enum V {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Re(String),
    Fn(Callback),
    Arr(Vec<V>),
    Obj(usize),
    Keywords(Arc<KeywordDict>),
}

impl V {
    fn truthy(&self) -> bool {
        match self {
            V::Null => false,
            V::Bool(b) => *b,
            V::Num(n) => *n != 0.0 && !n.is_nan(),
            V::Str(s) => !s.is_empty(),
            _ => true,
        }
    }

    fn source(&self) -> Option<String> {
        match self {
            V::Str(s) => Some(s.clone()),
            V::Re(s) => Some(s.clone()),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            V::Str(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Obj {
    props: IndexMap<String, V>,
    frozen: bool,
}

#[derive(Debug, Default)]
struct Heap {
    objs: Vec<Obj>,
}

impl Heap {
    fn get(&self, id: usize, key: &str) -> V {
        self.objs[id].props.get(key).cloned().unwrap_or(V::Null)
    }

    fn has(&self, id: usize, key: &str) -> bool {
        self.objs[id].props.get(key).is_some_and(|v| !matches!(v, V::Null))
    }

    fn truthy(&self, id: usize, key: &str) -> bool {
        self.objs[id].props.get(key).is_some_and(V::truthy)
    }

    /// Assignment; silently ignored on frozen objects (sloppy-mode JavaScript).
    fn set(&mut self, id: usize, key: &str, value: V) {
        if self.objs[id].frozen {
            return;
        }
        self.objs[id].props.insert(key.to_string(), value);
    }

    fn delete(&mut self, id: usize, key: &str) {
        if self.objs[id].frozen {
            return;
        }
        self.objs[id].props.shift_remove(key);
    }

    fn alloc(&mut self, obj: Obj) -> usize {
        self.objs.push(obj);
        self.objs.len() - 1
    }

    /// `inherit(original, ...objects)`: a new (unfrozen) object with the keys of all inputs.
    fn inherit(&mut self, original: usize, extra: &[&[(&str, V)]], more: &[usize]) -> usize {
        let mut props = self.objs[original].props.clone();
        for list in extra {
            for (k, v) in list.iter() {
                props.insert(k.to_string(), v.clone());
            }
        }
        for id in more {
            for (k, v) in &self.objs[*id].props {
                props.insert(k.clone(), v.clone());
            }
        }
        self.alloc(Obj { props, frozen: false })
    }
}

fn decode_value(json: &Json, base: usize) -> V {
    match json {
        Json::Null => V::Null,
        Json::Bool(b) => V::Bool(*b),
        Json::Number(n) => V::Num(n.as_f64().unwrap_or(0.0)),
        Json::String(s) => V::Str(s.clone()),
        Json::Array(items) => V::Arr(items.iter().map(|i| decode_value(i, base)).collect()),
        Json::Object(map) => {
            if let Some(id) = map.get("$ref").and_then(Json::as_u64) {
                return V::Obj(base + id as usize);
            }
            if let Some(source) = map.get("$re").and_then(Json::as_str) {
                return V::Re(source.to_string());
            }
            if let Some(name) = map.get("$fn").and_then(Json::as_str) {
                return V::Fn(match name {
                    "shebang" => Callback::Shebang,
                    "end_same_as_begin_begin" => Callback::EndSameAsBeginBegin,
                    "end_same_as_begin_end" => Callback::EndSameAsBeginEnd,
                    "jsx_is_truly_opening_tag" => Callback::JsxIsTrulyOpeningTag,
                    "before_match_extension" => Callback::BeforeMatchExtension,
                    _ => {
                        let set = map
                            .get("set")
                            .and_then(Json::as_array)
                            .map(|items| items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect())
                            .unwrap_or_default();
                        Callback::InSet(Arc::new(set))
                    }
                });
            }
            V::Null
        }
    }
}

// ============================================================================
// Keywords
// ============================================================================

/// keyword → (class name, relevance)
pub type KeywordDict = HashMap<String, (String, f64)>;

const COMMON_KEYWORDS: [&str; 11] = ["of", "and", "for", "in", "not", "or", "if", "then", "parent", "list", "value"];

fn score_for_keyword(keyword: &str, provided: Option<&str>) -> f64 {
    if let Some(score) = provided.filter(|s| !s.is_empty()) {
        return score.trim().parse::<f64>().unwrap_or(f64::NAN);
    }
    if COMMON_KEYWORDS.contains(&keyword.to_lowercase().as_str()) { 0.0 } else { 1.0 }
}

fn compile_keywords(heap: &Heap, raw: &V, case_insensitive: bool, class_name: &str, out: &mut KeywordDict) {
    let mut compile_list = |list: Vec<String>| {
        for keyword in list {
            let keyword = if case_insensitive { keyword.to_lowercase() } else { keyword };
            let mut pair = keyword.splitn(3, '|');
            let name = pair.next().unwrap_or_default().to_string();
            let score = score_for_keyword(&name, pair.next());
            out.insert(name, (class_name.to_string(), score));
        }
    };
    match raw {
        V::Str(s) => compile_list(s.split(' ').map(str::to_string).collect()),
        V::Arr(items) => compile_list(items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect()),
        V::Obj(id) => {
            for (key, value) in heap.objs[*id].props.clone() {
                compile_keywords(heap, &value, case_insensitive, &key, out);
            }
        }
        V::Keywords(dict) => {
            // Already compiled (shared keyword object compiled by another mode).
            for (k, v) in dict.iter() {
                out.insert(k.clone(), v.clone());
            }
        }
        _ => {}
    }
}

// ============================================================================
// Compiled grammar
// ============================================================================

#[derive(Debug)]
pub struct Rule {
    source: String,
    kind: RuleKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleKind {
    /// `begin` of `contains[i]` (mode id).
    Begin(usize),
    End,
    Illegal,
}

/// highlight.js `MultiRegex`: one alternation of all rules from `start`.
struct MultiRegex {
    regex: Option<fancy_regex::Regex>,
    /// capture group index → rule index (into the full rule list)
    match_indexes: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub enum SubLanguage {
    One(String),
    Many(Vec<String>),
}

pub struct CompiledMode {
    class_name: Option<String>,
    end_re: Option<fancy_regex::Regex>,
    keywords: Option<Arc<KeywordDict>>,
    keyword_pattern_re: Option<fancy_regex::Regex>,
    relevance: f64,
    starts: Option<usize>,
    ends_with_parent: bool,
    ends_parent: bool,
    return_begin: bool,
    return_end: bool,
    exclude_begin: bool,
    exclude_end: bool,
    skip: bool,
    sub_language: Option<SubLanguage>,
    end_same_as_begin: bool,
    on_begin: Option<Callback>,
    on_end: Option<Callback>,
    before_begin: Option<Callback>,
    rules: Vec<Rule>,
    begin_count: usize,
    matchers: Mutex<HashMap<usize, Arc<MultiRegex>>>,
    /// `opts.position` of each rule. Shared by all matchers of the mode and overwritten
    /// whenever a matcher including the rule is built, exactly like highlight.js.
    positions: Mutex<Vec<usize>>,
}

pub struct CompiledLanguage {
    pub name: String,
    case_insensitive: bool,
    class_name_aliases: HashMap<String, String>,
    root: usize,
    modes: HashMap<usize, CompiledMode>,
}

/// A registered (raw) language definition.
pub struct LanguageDef {
    pub name: String,
    pub aliases: Vec<String>,
    pub disable_autodetect: bool,
    pub superset_of: Option<String>,
    /// The definition's JSON object graph, kept as text and parsed when the language is first
    /// compiled: parsed `Value`s of all ~190 grammars take several times the memory of the text.
    source: Arc<str>,
    compiled: OnceLock<Result<Arc<CompiledLanguage>, String>>,
}

struct Compiler {
    heap: Heap,
    case_insensitive: bool,
    compiler_extensions: Vec<Callback>,
    modes: HashMap<usize, CompiledMode>,
    errors: Vec<String>,
}

fn either(sources: &[String]) -> String {
    format!("({})", sources.join("|"))
}

impl Compiler {
    fn lang_re(&mut self, source: &str) -> Option<fancy_regex::Regex> {
        match js_regex::compile(source, self.case_insensitive) {
            Ok(re) => Some(re),
            Err(e) => {
                self.errors.push(e);
                None
            }
        }
    }

    fn dependency_on_parent(&self, mode: usize) -> bool {
        if self.heap.truthy(mode, "endsWithParent") {
            return true;
        }
        match self.heap.get(mode, "starts") {
            V::Obj(s) => self.dependency_on_parent(s),
            _ => false,
        }
    }

    fn expand_or_clone_mode(&mut self, mode: usize) -> Vec<usize> {
        if self.heap.truthy(mode, "variants") && !self.heap.truthy(mode, "cachedVariants") {
            let variants = match self.heap.get(mode, "variants") {
                V::Arr(items) => items,
                _ => Vec::new(),
            };
            let mut cached = Vec::new();
            for variant in variants {
                if let V::Obj(variant) = variant {
                    let id = self.heap.inherit(mode, &[&[("variants", V::Null)]], &[variant]);
                    cached.push(V::Obj(id));
                }
            }
            self.heap.set(mode, "cachedVariants", V::Arr(cached));
        }
        if let V::Arr(cached) = self.heap.get(mode, "cachedVariants") {
            return cached.into_iter().filter_map(|v| if let V::Obj(id) = v { Some(id) } else { None }).collect();
        }
        if self.dependency_on_parent(mode) {
            let starts = match self.heap.get(mode, "starts") {
                V::Obj(s) => V::Obj(self.heap.inherit(s, &[], &[])),
                _ => V::Null,
            };
            return vec![self.heap.inherit(mode, &[&[("starts", starts)]], &[])];
        }
        if self.heap.objs[mode].frozen {
            return vec![self.heap.inherit(mode, &[], &[])];
        }
        vec![mode]
    }

    fn run_extension(&mut self, ext: &Callback, mode: usize) -> Result<(), String> {
        if let Callback::BeforeMatchExtension = ext {
            if !self.heap.truthy(mode, "beforeMatch") {
                return Ok(());
            }
            if self.heap.truthy(mode, "starts") {
                return Err("beforeMatch cannot be used with starts".into());
            }
            let original = self.heap.objs[mode].props.clone();
            let original_id = self.heap.alloc(Obj { props: original, frozen: false });
            self.heap.objs[mode].props.clear();
            let before = self.heap.get(original_id, "beforeMatch").source().unwrap_or_default();
            let begin = self.heap.get(original_id, "begin").source().unwrap_or_default();
            self.heap.set(mode, "begin", V::Str(format!("{before}(?={begin})")));
            self.heap.set(original_id, "endsParent", V::Bool(true));
            let starts = self.heap.alloc(Obj {
                props: IndexMap::from([
                    ("relevance".to_string(), V::Num(0.0)),
                    ("contains".to_string(), V::Arr(vec![V::Obj(original_id)])),
                ]),
                frozen: false,
            });
            self.heap.set(mode, "starts", V::Obj(starts));
            self.heap.set(mode, "relevance", V::Num(0.0));
            self.heap.delete(original_id, "beforeMatch");
        }
        Ok(())
    }

    fn compile_mode(&mut self, mode: usize, parent: Option<usize>) -> Result<(), String> {
        if self.heap.truthy(mode, "isCompiled") {
            return Ok(());
        }
        // compileMatch
        if self.heap.truthy(mode, "match") {
            if self.heap.truthy(mode, "begin") || self.heap.truthy(mode, "end") {
                return Err("begin & end are not supported with match".into());
            }
            let m = self.heap.get(mode, "match");
            self.heap.set(mode, "begin", m);
            self.heap.delete(mode, "match");
        }
        for ext in self.compiler_extensions.clone() {
            self.run_extension(&ext, mode)?;
        }
        self.heap.set(mode, "__beforeBegin", V::Null);

        // beginKeywords
        if parent.is_some() && self.heap.truthy(mode, "beginKeywords") {
            let begin_keywords = self.heap.get(mode, "beginKeywords").source().unwrap_or_default();
            let begin = format!("\\b({})(?!\\.)(?=\\b|\\s)", begin_keywords.split(' ').collect::<Vec<_>>().join("|"));
            self.heap.set(mode, "begin", V::Str(begin));
            self.heap.set(mode, "__beforeBegin", V::Fn(Callback::SkipIfHasPrecedingDot));
            if !self.heap.truthy(mode, "keywords") {
                let bk = self.heap.get(mode, "beginKeywords");
                self.heap.set(mode, "keywords", bk);
            }
            self.heap.delete(mode, "beginKeywords");
            if !self.heap.has(mode, "relevance") {
                self.heap.set(mode, "relevance", V::Num(0.0));
            }
        }
        // compileIllegal
        if let V::Arr(items) = self.heap.get(mode, "illegal") {
            let sources: Vec<String> = items.iter().filter_map(V::source).collect();
            self.heap.set(mode, "illegal", V::Str(either(&sources)));
        }
        // compileRelevance
        if !self.heap.has(mode, "relevance") {
            self.heap.set(mode, "relevance", V::Num(1.0));
        }
        self.heap.set(mode, "isCompiled", V::Bool(true));

        let mut keyword_pattern: V = V::Null;
        if let V::Obj(kw) = self.heap.get(mode, "keywords") {
            keyword_pattern = self.heap.get(kw, "$pattern");
            self.heap.delete(kw, "$pattern");
        }
        let mut keywords: Option<Arc<KeywordDict>> = None;
        let raw_keywords = self.heap.get(mode, "keywords");
        if raw_keywords.truthy() {
            let mut dict = KeywordDict::new();
            compile_keywords(&self.heap, &raw_keywords, self.case_insensitive, "keyword", &mut dict);
            let dict = Arc::new(dict);
            self.heap.set(mode, "keywords", V::Keywords(dict.clone()));
            keywords = Some(dict);
        }
        let lexemes = self.heap.get(mode, "lexemes");
        if lexemes.truthy() && keyword_pattern.truthy() {
            return Err("ERR: Prefer `keywords.$pattern` to `mode.lexemes`, BOTH are not allowed. (see mode reference) ".into());
        }
        let keyword_pattern_source = if keyword_pattern.truthy() {
            keyword_pattern.source()
        } else if lexemes.truthy() {
            lexemes.source()
        } else {
            None
        }
        .unwrap_or_else(|| "\\w+".to_string());
        let keyword_pattern_re = self.lang_re(&keyword_pattern_source);

        let mut end_re = None;
        if let Some(parent) = parent {
            if !self.heap.truthy(mode, "begin") {
                self.heap.set(mode, "begin", V::Re("\\B|\\b".into()));
            }
            if self.heap.truthy(mode, "endSameAsBegin") {
                let b = self.heap.get(mode, "begin");
                self.heap.set(mode, "end", b);
            }
            if !self.heap.truthy(mode, "end") && !self.heap.truthy(mode, "endsWithParent") {
                self.heap.set(mode, "end", V::Re("\\B|\\b".into()));
            }
            let end = self.heap.get(mode, "end");
            if end.truthy() {
                end_re = self.lang_re(&end.source().unwrap_or_default());
            }
            let mut terminator_end = end.source().unwrap_or_default();
            let parent_terminator = self.heap.get(parent, "terminatorEnd").source().unwrap_or_default();
            if self.heap.truthy(mode, "endsWithParent") && !parent_terminator.is_empty() {
                if end.truthy() {
                    terminator_end.push('|');
                }
                terminator_end.push_str(&parent_terminator);
            }
            self.heap.set(mode, "terminatorEnd", V::Str(terminator_end));
        }
        if !self.heap.truthy(mode, "contains") {
            self.heap.set(mode, "contains", V::Arr(Vec::new()));
        }

        let contains = match self.heap.get(mode, "contains") {
            V::Arr(items) => items,
            _ => Vec::new(),
        };
        let mut expanded: Vec<usize> = Vec::new();
        for c in contains {
            let target = match c {
                V::Str(s) if s == "self" => mode,
                V::Obj(id) => id,
                _ => continue,
            };
            expanded.extend(self.expand_or_clone_mode(target));
        }
        self.heap.set(mode, "contains", V::Arr(expanded.iter().map(|id| V::Obj(*id)).collect()));
        for c in &expanded {
            self.compile_mode(*c, Some(mode))?;
        }

        let starts = match self.heap.get(mode, "starts") {
            V::Obj(s) => Some(s),
            _ => None,
        };
        if let Some(starts) = starts {
            self.compile_mode(starts, parent)?;
        }

        // buildModeRegex
        let mut rules: Vec<Rule> = Vec::new();
        let mut begin_count = 0;
        for c in &expanded {
            let begin = self.heap.get(*c, "begin").source().unwrap_or_default();
            rules.push(Rule { source: begin, kind: RuleKind::Begin(*c) });
            begin_count += 1;
        }
        let terminator_end = self.heap.get(mode, "terminatorEnd").source().unwrap_or_default();
        if !terminator_end.is_empty() {
            rules.push(Rule { source: terminator_end, kind: RuleKind::End });
        }
        let illegal = self.heap.get(mode, "illegal");
        if illegal.truthy() {
            rules.push(Rule { source: illegal.source().unwrap_or_default(), kind: RuleKind::Illegal });
        }

        let callback = |v: V| if let V::Fn(cb) = v { Some(cb) } else { None };
        let sub_language = match self.heap.get(mode, "subLanguage") {
            V::Str(s) => Some(SubLanguage::One(s)),
            V::Arr(items) => {
                Some(SubLanguage::Many(items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect()))
            }
            _ => None,
        };
        let compiled = CompiledMode {
            class_name: self.heap.get(mode, "className").as_str().map(str::to_string).filter(|s| !s.is_empty()),
            end_re,
            keywords,
            keyword_pattern_re,
            relevance: match self.heap.get(mode, "relevance") {
                V::Num(n) => n,
                _ => 1.0,
            },
            starts,
            ends_with_parent: self.heap.truthy(mode, "endsWithParent"),
            ends_parent: self.heap.truthy(mode, "endsParent"),
            return_begin: self.heap.truthy(mode, "returnBegin"),
            return_end: self.heap.truthy(mode, "returnEnd"),
            exclude_begin: self.heap.truthy(mode, "excludeBegin"),
            exclude_end: self.heap.truthy(mode, "excludeEnd"),
            skip: self.heap.truthy(mode, "skip"),
            sub_language,
            end_same_as_begin: self.heap.truthy(mode, "endSameAsBegin"),
            on_begin: callback(self.heap.get(mode, "on:begin")),
            on_end: callback(self.heap.get(mode, "on:end")),
            before_begin: callback(self.heap.get(mode, "__beforeBegin")),
            positions: Mutex::new(vec![0; rules.len()]),
            rules,
            begin_count,
            matchers: Mutex::new(HashMap::new()),
        };
        self.modes.insert(mode, compiled);
        Ok(())
    }
}

impl LanguageDef {
    fn from_source(source: Arc<str>) -> Option<Self> {
        let json: Json = serde_json::from_str(&source).ok()?;
        let name = json.get("name")?.as_str()?.to_string();
        let nodes = json.get("nodes")?.as_array()?;
        let root = json.get("root")?.get("$ref")?.as_u64()? as usize;
        let root_node = nodes.get(root)?;
        let aliases = match root_node.get("aliases") {
            Some(Json::Array(items)) => items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect(),
            Some(Json::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        let disable_autodetect = root_node.get("disableAutodetect").and_then(Json::as_bool).unwrap_or(false);
        let superset_of = root_node.get("supersetOf").and_then(Json::as_str).map(str::to_string);
        Some(Self { name, aliases, disable_autodetect, superset_of, source, compiled: OnceLock::new() })
    }

    /// Compile the language (once).
    pub fn compiled(&self) -> Result<Arc<CompiledLanguage>, String> {
        self.compiled.get_or_init(|| self.compile()).clone()
    }

    fn compile(&self) -> Result<Arc<CompiledLanguage>, String> {
        let mut json: Json = serde_json::from_str(&self.source).unwrap_or_default();
        let root = json.get("root").and_then(|r| r.get("$ref")).and_then(Json::as_u64).unwrap_or(0) as usize;
        let nodes = match json.get_mut("nodes").map(Json::take) {
            Some(Json::Array(nodes)) => nodes,
            _ => Vec::new(),
        };
        let mut heap = Heap::default();
        for node in &nodes {
            let mut obj = Obj::default();
            if let Json::Object(map) = node {
                for (k, v) in map {
                    if k == "$frozen" {
                        obj.frozen = v.as_bool().unwrap_or(false);
                        continue;
                    }
                    obj.props.insert(k.clone(), decode_value(v, 0));
                }
            }
            heap.objs.push(obj);
        }
        // `self` is not valid at the top-level
        if let V::Arr(items) = heap.get(root, "contains")
            && items.iter().any(|i| matches!(i, V::Str(s) if s == "self"))
        {
            return Err("ERR: contains `self` is not supported at the top-level of a language.  See documentation.".into());
        }
        let case_insensitive = heap.truthy(root, "case_insensitive");
        let compiler_extensions = match heap.get(root, "compilerExtensions") {
            V::Arr(items) => items.into_iter().filter_map(|i| if let V::Fn(cb) = i { Some(cb) } else { None }).collect(),
            _ => Vec::new(),
        };
        let class_name_aliases = match heap.get(root, "classNameAliases") {
            V::Obj(id) => heap.objs[id]
                .props
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
            _ => HashMap::new(),
        };
        let mut compiler =
            Compiler { heap, case_insensitive, compiler_extensions, modes: HashMap::new(), errors: Vec::new() };
        compiler.compile_mode(root, None)?;
        Ok(Arc::new(CompiledLanguage {
            name: self.name.clone(),
            case_insensitive,
            class_name_aliases,
            root,
            modes: compiler.modes,
        }))
    }
}

// ============================================================================
// Registry
// ============================================================================

static LANGUAGE_DATA: &[u8] = include_bytes!("languages.json.gz");

/// The languages registered at startup (in registration order).
pub const EAGER_LANGUAGES: [&str; 21] = [
    "python",
    "java",
    "go",
    "javascript",
    "json",
    "cpp",
    "typescript",
    "php",
    "ruby",
    "c",
    "csharp",
    "nix",
    "bash",
    "rust",
    "scala",
    "kotlin",
    "swift",
    "dart",
    "groovy",
    "perl",
    "lua",
];

static ALL_DEFINITIONS: LazyLock<Vec<Arc<LanguageDef>>> = LazyLock::new(|| {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(LANGUAGE_DATA);
    let mut text = String::new();
    if decoder.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    let Ok(items) = serde_json::from_str::<Vec<&serde_json::value::RawValue>>(&text) else {
        return Vec::new();
    };
    items.into_iter().filter_map(|item| LanguageDef::from_source(Arc::from(item.get()))).map(Arc::new).collect()
});

#[derive(Default)]
struct Registry {
    languages: IndexMap<String, Arc<LanguageDef>>,
    aliases: HashMap<String, String>,
}

impl Registry {
    fn register(&mut self, def: Arc<LanguageDef>) {
        for alias in &def.aliases {
            self.aliases.insert(alias.to_lowercase(), def.name.clone());
        }
        // Re-registering keeps the original position (JavaScript object key order).
        if let Some(slot) = self.languages.get_mut(&def.name) {
            *slot = def;
        } else {
            self.languages.insert(def.name.clone(), def);
        }
    }

    fn get(&self, name: &str) -> Option<Arc<LanguageDef>> {
        let name = name.to_lowercase();
        self.languages
            .get(&name)
            .cloned()
            .or_else(|| self.aliases.get(&name).and_then(|n| self.languages.get(n)).cloned())
    }
}

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(|| {
    let mut registry = Registry::default();
    for name in EAGER_LANGUAGES {
        if let Some(def) = ALL_DEFINITIONS.iter().find(|d| d.name == name) {
            registry.register(def.clone());
        }
    }
    RwLock::new(registry)
});

/// Register every bundled language (`import("highlight.js/lib/index.js")`).
pub fn register_all_languages() {
    let mut registry = REGISTRY.write();
    for def in ALL_DEFINITIONS.iter() {
        if !registry.languages.contains_key(&def.name) {
            registry.register(def.clone());
        } else {
            // lib/index.js re-registers every language with a fresh definition.
            let fresh = LanguageDef::from_source(def.source.clone()).map(Arc::new);
            if let Some(fresh) = fresh {
                registry.register(fresh);
            }
        }
    }
}

pub fn get_language(name: &str) -> Option<Arc<LanguageDef>> {
    REGISTRY.read().get(name)
}

fn language_names() -> Vec<String> {
    REGISTRY.read().languages.keys().cloned().collect()
}

fn auto_detection(name: &str) -> bool {
    get_language(name).is_some_and(|l| !l.disable_autodetect)
}

// ============================================================================
// Emitter (TokenTree) and HTML renderer
// ============================================================================

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Group { kind: Option<String>, sublanguage: bool, children: Vec<Node> },
}

#[derive(Debug, Clone)]
pub struct Emitter {
    root: Vec<Node>,
    /// Path of child indexes from the root to the open node.
    stack: Vec<usize>,
}

impl Emitter {
    fn new() -> Self {
        Self { root: Vec::new(), stack: Vec::new() }
    }

    fn top_children(&mut self) -> &mut Vec<Node> {
        let mut children = &mut self.root;
        for &index in &self.stack {
            match &mut children[index] {
                Node::Group { children: c, .. } => children = c,
                Node::Text(_) => unreachable!("text nodes have no children"),
            }
        }
        children
    }

    fn add(&mut self, node: Node) {
        self.top_children().push(node);
    }

    fn open_node(&mut self, kind: &str) {
        let children = self.top_children();
        children.push(Node::Group { kind: Some(kind.to_string()), sublanguage: false, children: Vec::new() });
        let index = children.len() - 1;
        self.stack.push(index);
    }

    fn close_node(&mut self) -> bool {
        self.stack.pop().is_some()
    }

    fn close_all_nodes(&mut self) {
        while self.close_node() {}
    }

    fn add_keyword(&mut self, text: &str, kind: &str) {
        if text.is_empty() {
            return;
        }
        self.open_node(kind);
        self.add_text(text);
        self.close_node();
    }

    fn add_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.add(Node::Text(text.to_string()));
    }

    fn add_sublanguage(&mut self, emitter: Emitter, name: Option<&str>) {
        self.add(Node::Group { kind: name.map(str::to_string), sublanguage: true, children: emitter.root });
    }

    fn to_html(&self) -> String {
        fn walk(out: &mut String, node: &Node) {
            match node {
                Node::Text(text) => out.push_str(&escape_html(text)),
                Node::Group { kind, sublanguage, children } => {
                    if let Some(kind) = kind.as_deref().filter(|k| !k.is_empty()) {
                        if *sublanguage {
                            out.push_str(&format!("<span class=\"{kind}\">"));
                        } else {
                            out.push_str(&format!("<span class=\"hljs-{kind}\">"));
                        }
                    }
                    for child in children {
                        walk(out, child);
                    }
                    if kind.as_deref().is_some_and(|k| !k.is_empty()) {
                        out.push_str("</span>");
                    }
                }
            }
        }
        let mut out = String::new();
        for node in &self.root {
            walk(&mut out, node);
        }
        out
    }
}

pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

// ============================================================================
// Highlighter
// ============================================================================

/// A match of a mode's multi-regex, re-indexed to the matched rule (`match.splice(0, i)`).
#[derive(Debug, Clone)]
struct RuleMatch {
    /// Byte index of the match in the code.
    index: usize,
    /// Groups of the rule (`[0]` is the whole match).
    groups: Vec<Option<String>>,
    kind: RuleKind,
    /// Position of the rule in the multi-regex (`opts.position`).
    position: usize,
}

impl RuleMatch {
    fn text(&self) -> &str {
        self.groups.first().and_then(|g| g.as_deref()).unwrap_or("")
    }
}

/// Per-call mutable state of compiled modes (matcher cursors, `data`, `endRe` overrides).
#[derive(Default)]
struct ModeState {
    regex_index: usize,
    last_index: usize,
    begin_match: Option<Option<String>>,
    end_re_override: Option<Arc<fancy_regex::Regex>>,
}

/// A mode frame on the highlighter stack (`Object.create(mode, { parent })`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    mode: usize,
}

pub struct HighlightResult {
    pub language: Option<String>,
    pub relevance: f64,
    pub value: String,
    pub illegal: bool,
    emitter: Emitter,
    /// Frame chain (root first) for continuations.
    top: Vec<usize>,
}

impl CompiledMode {
    fn matcher(&self, start: usize, case_insensitive: bool) -> Arc<MultiRegex> {
        if let Some(m) = self.matchers.lock().get(&start) {
            return m.clone();
        }
        let rules = &self.rules[start.min(self.rules.len())..];
        {
            let mut positions = self.positions.lock();
            for offset in 0..rules.len() {
                positions[start + offset] = offset;
            }
        }
        let matcher = if rules.is_empty() {
            MultiRegex { regex: None, match_indexes: Vec::new() }
        } else {
            // join(): wrap each rule in a group, renumbering backreferences.
            let mut num_captures = 0usize;
            let mut parts = Vec::new();
            let mut match_indexes = Vec::new();
            let mut match_at = 1usize;
            for (offset, rule) in rules.iter().enumerate() {
                num_captures += 1;
                let shift = num_captures;
                parts.push(format!("({})", renumber_backrefs(&rule.source, shift, &mut num_captures)));
                match_indexes.push((match_at, start + offset));
                match_at += js_regex::count_capture_groups(&rule.source) + 1;
            }
            let joined = parts.join("|");
            let regex = js_regex::compile(&joined, case_insensitive).ok();
            MultiRegex { regex, match_indexes }
        };
        let matcher = Arc::new(matcher);
        self.matchers.lock().insert(start, matcher.clone());
        matcher
    }
}

/// `join()`'s backreference renumbering for one regex.
fn renumber_backrefs(source: &str, offset: usize, num_captures: &mut usize) -> String {
    static BACKREF_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\[(?:[^\\\]]|\\.)*\]|\(\??|\\([1-9][0-9]*)|\\.").expect("valid regex")
    });
    let mut out = String::new();
    let mut rest = source;
    loop {
        let Some(m) = BACKREF_RE.captures(rest) else {
            out.push_str(rest);
            break;
        };
        let whole = m.get(0).expect("match");
        out.push_str(&rest[..whole.start()]);
        let text = whole.as_str();
        if text.starts_with('\\')
            && let Some(n) = m.get(1)
        {
            let number: usize = n.as_str().parse().unwrap_or(0);
            out.push_str(&format!("\\{}", number + offset));
        } else {
            out.push_str(text);
            if text == "(" {
                *num_captures += 1;
            }
        }
        rest = &rest[whole.end()..];
    }
    out
}

fn exec_multi(matcher: &MultiRegex, mode: &CompiledMode, code: &str, last_index: usize) -> Option<RuleMatch> {
    let regex = matcher.regex.as_ref()?;
    if last_index > code.len() {
        return None;
    }
    let caps = regex.captures_from_pos(code, last_index).ok()??;
    let whole = caps.get(0)?;
    // The first participating rule group.
    let (group, rule_index) =
        matcher.match_indexes.iter().copied().find(|(group, _)| caps.get(*group).is_some())?;
    let rule = &mode.rules[rule_index];
    let group_count = js_regex::count_capture_groups(&rule.source);
    let groups = (0..=group_count).map(|k| caps.get(group + k).map(|m| m.as_str().to_string())).collect();
    let position = mode.positions.lock()[rule_index];
    Some(RuleMatch { index: whole.start(), groups, kind: rule.kind, position })
}

fn next_char_boundary(text: &str, index: usize) -> usize {
    let mut i = index + 1;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i.min(text.len())
}

/// `ResumableMultiRegex.exec`.
fn exec_resumable(
    lang: &CompiledLanguage,
    mode_id: usize,
    state: &mut ModeState,
    code: &str,
) -> Option<RuleMatch> {
    let mode = &lang.modes[&mode_id];
    let m = mode.matcher(state.regex_index, lang.case_insensitive);
    let mut result = exec_multi(&m, mode, code, state.last_index);
    if state.regex_index != 0 && !result.as_ref().is_some_and(|r| r.index == state.last_index) {
        let m2 = mode.matcher(0, lang.case_insensitive);
        result = exec_multi(&m2, mode, code, next_char_boundary(code, state.last_index));
    }
    if let Some(r) = &result {
        state.regex_index += r.position + 1;
        if state.regex_index == mode.begin_count {
            // wrap-around to considering all matches again
            state.regex_index = 0;
        }
    }
    result
}

struct Highlighter<'c> {
    lang: Arc<CompiledLanguage>,
    code: &'c str,
    ignore_illegals: bool,
    /// Per-call mode state (matcher cursors, `data`, `endRe` overrides).
    states: HashMap<usize, ModeState>,
    /// Sub-language continuations of this call (language name → frame chain).
    continuations: HashMap<String, Vec<usize>>,
    emitter: Emitter,
    stack: Vec<Frame>,
    mode_buffer: String,
    relevance: f64,
    resume_scan_at_same_position: bool,
}

enum Outcome {
    Advance(usize),
    NoMatch,
}

impl Highlighter<'_> {
    fn mode(&self, id: usize) -> &CompiledMode {
        &self.lang.modes[&id]
    }

    fn top(&self) -> usize {
        self.stack.last().map(|f| f.mode).unwrap_or(self.lang.root)
    }

    fn state(&mut self, mode: usize) -> &mut ModeState {
        self.states.entry(mode).or_default()
    }

    fn keyword_data(&self, mode: &CompiledMode, text: &str) -> Option<(String, f64)> {
        let key = if self.lang.case_insensitive { text.to_lowercase() } else { text.to_string() };
        mode.keywords.as_ref()?.get(&key).cloned()
    }

    fn process_keywords(&mut self) {
        let top_id = self.top();
        let lang = self.lang.clone();
        let top = &lang.modes[&top_id];
        let buffer = std::mem::take(&mut self.mode_buffer);
        if top.keywords.is_none() {
            self.emitter.add_text(&buffer);
            return;
        }
        let Some(re) = &top.keyword_pattern_re else {
            self.emitter.add_text(&buffer);
            return;
        };
        let mut last_index = 0;
        let mut buf = String::new();
        let mut pos = 0;
        while pos <= buffer.len() {
            let Ok(Some(m)) = re.find_from_pos(&buffer, pos) else { break };
            buf.push_str(&buffer[last_index..m.start()]);
            match self.keyword_data(top, m.as_str()) {
                Some((kind, keyword_relevance)) => {
                    self.emitter.add_text(&buf);
                    buf.clear();
                    self.relevance += keyword_relevance;
                    if kind.starts_with('_') {
                        // _ implied for relevance only, do not highlight by applying a class name
                        buf.push_str(m.as_str());
                    } else {
                        let css_class = lang.class_name_aliases.get(&kind).cloned().unwrap_or(kind);
                        self.emitter.add_keyword(m.as_str(), &css_class);
                    }
                }
                None => buf.push_str(m.as_str()),
            }
            last_index = m.end();
            pos = if m.end() == m.start() { next_char_boundary(&buffer, m.end()) } else { m.end() };
            if m.end() == buffer.len() && m.end() == m.start() {
                break;
            }
        }
        buf.push_str(&buffer[last_index.min(buffer.len())..]);
        self.emitter.add_text(&buf);
    }

    fn process_sub_language(&mut self) {
        if self.mode_buffer.is_empty() {
            return;
        }
        let top_id = self.top();
        let lang = self.lang.clone();
        let top = &lang.modes[&top_id];
        let buffer = std::mem::take(&mut self.mode_buffer);
        let result = match top.sub_language.clone() {
            Some(SubLanguage::One(name)) => {
                if REGISTRY.read().languages.get(&name).is_none() {
                    self.emitter.add_text(&buffer);
                    return;
                }
                let continuation = self.continuations.get(&name).cloned();
                let result = highlight_inner(&name, &buffer, true, continuation);
                if let Ok(result) = &result {
                    self.continuations.insert(name.clone(), result.top.clone());
                }
                match result {
                    Ok(result) => result,
                    Err(_) => {
                        self.emitter.add_text(&buffer);
                        return;
                    }
                }
            }
            Some(SubLanguage::Many(list)) => {
                highlight_auto(&buffer, if list.is_empty() { None } else { Some(list) })
            }
            None => return,
        };
        // Counting embedded language score towards the host language may be disabled
        // with zeroing the containing mode relevance.
        if top.relevance > 0.0 {
            self.relevance += result.relevance;
        }
        self.emitter.add_sublanguage(result.emitter, result.language.as_deref());
    }

    fn process_buffer(&mut self) {
        let top = self.top();
        if self.mode(top).sub_language.is_some() {
            self.process_sub_language();
        } else {
            self.process_keywords();
        }
        self.mode_buffer.clear();
    }

    fn start_new_mode(&mut self, mode: usize) {
        if let Some(class_name) = self.mode(mode).class_name.clone() {
            let kind = self.lang.class_name_aliases.get(&class_name).cloned().unwrap_or(class_name);
            self.emitter.open_node(&kind);
        }
        self.stack.push(Frame { mode });
    }

    fn run_callback(&mut self, callback: &Callback, mode: usize, m: &RuleMatch) -> bool {
        // Returns true when the match is ignored.
        match callback {
            Callback::Shebang => m.index != 0,
            Callback::SkipIfHasPrecedingDot => m.index > 0 && self.code.as_bytes()[m.index - 1] == b'.',
            Callback::EndSameAsBeginBegin => {
                let group = m.groups.get(1).cloned().flatten();
                self.state(mode).begin_match = Some(group);
                false
            }
            Callback::EndSameAsBeginEnd => {
                let group = m.groups.get(1).cloned().flatten();
                self.state(mode).begin_match.clone().unwrap_or(None) != group
            }
            Callback::JsxIsTrulyOpeningTag => {
                let after = m.index + m.text().len();
                let next = self.code[after..].chars().next();
                if next == Some('<') {
                    return true;
                }
                if next == Some('>') {
                    let tag = format!("</{}", &m.text()[1..]);
                    return !self.code[after..].contains(&tag);
                }
                false
            }
            Callback::InSet(set) => !set.contains(m.text()),
            Callback::BeforeMatchExtension => false,
        }
    }

    fn do_ignore(&mut self, m: &RuleMatch) -> usize {
        let top = self.top();
        if self.state(top).regex_index == 0 {
            // no more regexs to potentially match here, so we move the cursor forward one space
            let end = next_char_boundary(self.code, m.index);
            self.mode_buffer.push_str(&self.code[m.index.min(self.code.len())..end]);
            end - m.index
        } else {
            // no need to move the cursor, we still have additional regexes to try and match at this very spot
            self.resume_scan_at_same_position = true;
            0
        }
    }

    fn do_begin_match(&mut self, m: &RuleMatch, new_mode: usize) -> usize {
        let lexeme = m.text().to_string();
        let callbacks = [self.mode(new_mode).before_begin.clone(), self.mode(new_mode).on_begin.clone()];
        for callback in callbacks.into_iter().flatten() {
            if self.run_callback(&callback, new_mode, m) {
                return self.do_ignore(m);
            }
        }
        if self.mode(new_mode).end_same_as_begin {
            let escaped = js_regex::escape(&lexeme);
            if let Ok(re) = js_regex::compile(&escaped, false) {
                self.state(new_mode).end_re_override = Some(Arc::new(re));
            }
        }
        let (skip, exclude_begin, return_begin) = {
            let mode = self.mode(new_mode);
            (mode.skip, mode.exclude_begin, mode.return_begin)
        };
        if skip {
            self.mode_buffer.push_str(&lexeme);
        } else {
            if exclude_begin {
                self.mode_buffer.push_str(&lexeme);
            }
            self.process_buffer();
            if !return_begin && !exclude_begin {
                self.mode_buffer = lexeme.clone();
            }
        }
        self.start_new_mode(new_mode);
        if return_begin { 0 } else { lexeme.len() }
    }

    /// `endOfMode`: the index of the frame whose end matched.
    fn end_of_mode(&mut self, frame_index: usize, m: &RuleMatch, match_plus_remainder: &str) -> Option<usize> {
        let mode_id = self.stack[frame_index].mode;
        let end_re = match self.state(mode_id).end_re_override.clone() {
            Some(re) => Some(re),
            None => self.mode(mode_id).end_re.as_ref().map(|r| Arc::new(r.clone())),
        };
        let mut matched = end_re
            .as_ref()
            .and_then(|re| re.find(match_plus_remainder).ok().flatten())
            .is_some_and(|found| found.start() == 0);
        if matched {
            if let Some(on_end) = self.mode(mode_id).on_end.clone()
                && self.run_callback(&on_end, mode_id, m)
            {
                matched = false;
            }
            if matched {
                let mut index = frame_index;
                while self.mode(self.stack[index].mode).ends_parent && index > 0 {
                    index -= 1;
                }
                return Some(index);
            }
        }
        // even if on:end fires an `ignore` it's still possible that we might trigger the end
        // node because of a parent mode
        if self.mode(mode_id).ends_with_parent && frame_index > 0 {
            return self.end_of_mode(frame_index - 1, m, match_plus_remainder);
        }
        None
    }

    fn do_end_match(&mut self, m: &RuleMatch) -> Outcome {
        let lexeme = m.text().to_string();
        let match_plus_remainder = self.code[m.index..].to_string();
        let Some(end_index) = self.end_of_mode(self.stack.len() - 1, m, &match_plus_remainder) else {
            return Outcome::NoMatch;
        };
        let origin_id = self.top();
        let (skip, return_end, exclude_end) = {
            let origin = self.mode(origin_id);
            (origin.skip, origin.return_end, origin.exclude_end)
        };
        if skip {
            self.mode_buffer.push_str(&lexeme);
        } else {
            if !(return_end || exclude_end) {
                self.mode_buffer.push_str(&lexeme);
            }
            self.process_buffer();
            if exclude_end {
                self.mode_buffer = lexeme.clone();
            }
        }
        let end_mode = self.stack[end_index].mode;
        while self.stack.len() > end_index {
            let top = self.top();
            if self.mode(top).class_name.is_some() {
                self.emitter.close_node();
            }
            let (skip, sub_language, relevance) = {
                let mode = self.mode(top);
                (mode.skip, mode.sub_language.is_some(), mode.relevance)
            };
            if !skip && !sub_language {
                self.relevance += relevance;
            }
            self.stack.pop();
        }
        if let Some(starts) = self.mode(end_mode).starts {
            if self.mode(end_mode).end_same_as_begin {
                let end_re = self.state(end_mode).end_re_override.clone();
                self.state(starts).end_re_override = end_re;
            }
            self.start_new_mode(starts);
        }
        Outcome::Advance(if return_end { 0 } else { lexeme.len() })
    }

    fn process_lexeme(&mut self, text_before_match: &str, m: Option<&RuleMatch>, last_match: &mut Option<RuleMatch>, iterations: usize) -> Result<usize, String> {
        self.mode_buffer.push_str(text_before_match);
        let Some(m) = m else {
            self.process_buffer();
            return Ok(0);
        };
        let lexeme = m.text().to_string();

        // we've found a 0 width match and we're stuck, so we need to advance
        if let Some(last) = last_match.as_ref()
            && matches!(last.kind, RuleKind::Begin(_))
            && m.kind == RuleKind::End
            && last.index == m.index
            && lexeme.is_empty()
        {
            // spit the "skipped" character that our regex choked on back into the output sequence
            let end = next_char_boundary(self.code, m.index);
            self.mode_buffer.push_str(&self.code[m.index.min(self.code.len())..end]);
            return Ok(end - m.index);
        }
        *last_match = Some(m.clone());

        match m.kind {
            RuleKind::Begin(rule) => return Ok(self.do_begin_match(m, rule)),
            RuleKind::Illegal if !self.ignore_illegals => {
                let top = self.top();
                let class_name = self.mode(top).class_name.clone().unwrap_or_else(|| "<unnamed>".to_string());
                return Err(format!("Illegal lexeme \"{lexeme}\" for mode \"{class_name}\""));
            }
            RuleKind::End => {
                if let Outcome::Advance(n) = self.do_end_match(m) {
                    return Ok(n);
                }
            }
            RuleKind::Illegal => {}
        }

        // edge case for when illegal matches $ (end of line) which is technically a 0 width match
        if m.kind == RuleKind::Illegal && lexeme.is_empty() {
            return Ok(next_char_boundary(self.code, m.index) - m.index);
        }

        if iterations > 100000 && iterations > m.index * 3 {
            return Err("potential infinite loop, way more iterations than matches".into());
        }

        self.mode_buffer.push_str(&lexeme);
        Ok(lexeme.len())
    }

    fn run(&mut self) -> Result<(), String> {
        let mut index = 0usize;
        let mut iterations = 0usize;
        let mut last_match: Option<RuleMatch> = None;
        let top = self.top();
        self.state(top).regex_index = 0;
        loop {
            iterations += 1;
            let top = self.top();
            if self.resume_scan_at_same_position {
                self.resume_scan_at_same_position = false;
            } else {
                self.state(top).regex_index = 0;
            }
            self.state(top).last_index = index;
            let lang = self.lang.clone();
            let mut state = std::mem::take(self.state(top));
            let m = exec_resumable(&lang, top, &mut state, self.code);
            *self.state(top) = state;
            let Some(m) = m else { break };
            let before = self.code[index.min(m.index)..m.index].to_string();
            let processed = self.process_lexeme(&before, Some(&m), &mut last_match, iterations)?;
            index = m.index + processed;
        }
        let rest = self.code[index.min(self.code.len())..].to_string();
        self.process_lexeme(&rest, None, &mut last_match, iterations)?;
        Ok(())
    }
}

fn highlight_inner(
    language_name: &str,
    code: &str,
    ignore_illegals: bool,
    continuation: Option<Vec<usize>>,
) -> Result<HighlightResult, String> {
    let Some(def) = get_language(language_name) else {
        return Err(format!("Unknown language: \"{language_name}\""));
    };
    let lang = def.compiled()?;

    let stack: Vec<Frame> = match continuation {
        Some(chain) if !chain.is_empty() => chain.into_iter().map(|mode| Frame { mode }).collect(),
        _ => vec![Frame { mode: lang.root }],
    };
    let mut highlighter = Highlighter {
        lang: lang.clone(),
        code,
        ignore_illegals,
        states: HashMap::new(),
        continuations: HashMap::new(),
        emitter: Emitter::new(),
        stack,
        mode_buffer: String::new(),
        relevance: 0.0,
        resume_scan_at_same_position: false,
    };

    // processContinuations
    let frames: Vec<usize> = highlighter.stack.iter().skip(1).map(|f| f.mode).collect();
    for mode in frames {
        if let Some(class_name) = highlighter.mode(mode).class_name.clone() {
            highlighter.emitter.open_node(&class_name);
        }
    }

    let top: Vec<usize>;
    match highlighter.run() {
        Ok(()) => {
            highlighter.emitter.close_all_nodes();
            top = highlighter.stack.iter().map(|f| f.mode).collect();
            Ok(HighlightResult {
                language: Some(language_name.to_string()),
                relevance: highlighter.relevance.floor(),
                value: highlighter.emitter.to_html(),
                illegal: false,
                emitter: highlighter.emitter,
                top,
            })
        }
        Err(message) if message.contains("Illegal") => Ok(HighlightResult {
            language: None,
            relevance: 0.0,
            value: escape_html(code),
            illegal: true,
            emitter: highlighter.emitter,
            top: Vec::new(),
        }),
        Err(_) => {
            // SAFE_MODE: return the escaped code.
            top = highlighter.stack.iter().map(|f| f.mode).collect();
            Ok(HighlightResult {
                language: Some(language_name.to_string()),
                relevance: 0.0,
                value: escape_html(code),
                illegal: false,
                emitter: highlighter.emitter,
                top,
            })
        }
    }
}

fn just_text_highlight_result(code: &str) -> HighlightResult {
    let mut emitter = Emitter::new();
    emitter.add_text(code);
    HighlightResult { language: None, relevance: 0.0, value: escape_html(code), illegal: false, emitter, top: Vec::new() }
}

/// `hljs.highlightAuto(code, languageSubset)`.
pub fn highlight_auto(code: &str, language_subset: Option<Vec<String>>) -> HighlightResult {
    let subset = language_subset.unwrap_or_else(language_names);
    let mut results = vec![just_text_highlight_result(code)];
    for name in subset {
        if get_language(&name).is_none() || !auto_detection(&name) {
            continue;
        }
        if let Ok(result) = highlight_inner(&name, code, false, None) {
            results.push(result);
        }
    }
    // Stable sort by relevance; ties go to the base language of a superset.
    let superset_of = |name: &Option<String>| -> Option<String> {
        name.as_ref().and_then(|n| get_language(n)).and_then(|l| l.superset_of.clone())
    };
    results.sort_by(|a, b| {
        if a.relevance != b.relevance {
            return b.relevance.partial_cmp(&a.relevance).unwrap_or(std::cmp::Ordering::Equal);
        }
        if let (Some(a_lang), Some(b_lang)) = (&a.language, &b.language) {
            if superset_of(&a.language).as_deref() == Some(b_lang.as_str()) {
                return std::cmp::Ordering::Greater;
            } else if superset_of(&b.language).as_deref() == Some(a_lang.as_str()) {
                return std::cmp::Ordering::Less;
            }
        }
        std::cmp::Ordering::Equal
    });
    results.swap_remove(0)
}

/// `hljs.highlight(code, { language, ignoreIllegals })`.
pub fn highlight(code: &str, language: &str, ignore_illegals: bool) -> Result<HighlightResult, String> {
    highlight_inner(language, code, ignore_illegals, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bundled grammar compiles and every rule regex translates to a valid pattern.
    #[test]
    fn all_grammars_compile() {
        register_all_languages();
        let mut failures = Vec::new();
        for def in ALL_DEFINITIONS.iter() {
            let lang = match def.compiled() {
                Ok(lang) => lang,
                Err(e) => {
                    failures.push(format!("{}: {e}", def.name));
                    continue;
                }
            };
            for (id, mode) in &lang.modes {
                for rule in &mode.rules {
                    if let Err(e) = js_regex::compile(&rule.source, lang.case_insensitive) {
                        failures.push(format!("{} mode {id}: {e}", def.name));
                    }
                }
                let matcher = mode.matcher(0, lang.case_insensitive);
                if !mode.rules.is_empty() && matcher.regex.is_none() {
                    failures.push(format!("{} mode {id}: combined matcher failed", def.name));
                }
            }
        }
        assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn highlights_typescript_like_highlight_js() {
        let result = highlight("const value = 1", "typescript", true).unwrap();
        assert_eq!(
            result.value,
            "<span class=\"hljs-keyword\">const</span> value = <span class=\"hljs-number\">1</span>"
        );
    }

    #[test]
    fn highlights_python_docstrings_and_decorators() {
        let result = highlight("\"\"\"\nline one\n\"\"\"\n@decorator\ndef f(): pass", "python", true).unwrap();
        assert!(result.value.starts_with("<span class=\"hljs-string\">&quot;&quot;&quot;\nline one\n&quot;&quot;&quot;</span>"), "{}", result.value);
        assert!(result.value.contains("<span class=\"hljs-meta\">@decorator</span>"), "{}", result.value);
    }

    /// Differential test against highlight.js 10.7.3 output
    /// (`scripts/highlightjs-samples.js`).
    #[test]
    fn matches_highlight_js_output() {
        register_all_languages();
        let fixture: Json = serde_json::from_str(include_str!("testdata/hljs-samples.json")).unwrap();
        let mut failures = Vec::new();
        for sample in fixture["highlight"].as_array().unwrap() {
            let lang = sample["lang"].as_str().unwrap();
            let code = sample["code"].as_str().unwrap();
            let expected = sample["value"].as_str().unwrap();
            let actual = highlight(code, lang, true).map(|r| r.value).unwrap_or_else(|e| e);
            if actual != expected {
                failures.push(format!("{lang}:\n expected: {expected:?}\n   actual: {actual:?}"));
            }
        }
        for sample in fixture["auto"].as_array().unwrap() {
            let code = sample["code"].as_str().unwrap();
            let result = highlight_auto(code, None);
            if result.language.as_deref() != sample["language"].as_str() || result.value != sample["value"].as_str().unwrap() {
                failures.push(format!(
                    "auto {code:?}: expected {:?} got {:?} ({} vs {})",
                    sample["language"], result.language, sample["relevance"], result.relevance
                ));
            }
        }
        let subset = &fixture["autoSubset"];
        let list: Vec<String> =
            subset["subset"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        let result = highlight_auto(subset["code"].as_str().unwrap(), Some(list));
        if result.language.as_deref() != subset["language"].as_str() {
            failures.push(format!("auto subset: expected {:?} got {:?}", subset["language"], result.language));
        }
        assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
    }
}
