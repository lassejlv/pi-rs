//! Port of `yaml` 2.9.0 `Document#toJS` (`dist/nodes/{toJS,addPairToJSMap,Alias,YAMLMap,YAMLSeq}.js`,
//! `YAMLSet`/`YAMLOMap` `toJSON`, explicit `!!merge` keys) and of `JSON.stringify` for the result.

use std::collections::HashMap;

use serde_json::{Map, Number, Value};

use super::compose::{Kind, MapClass, Node, NodeId, SVal, SeqClass, SeqItem, ScalarType};

/// A JavaScript value produced by `toJS`. Objects carry an identity for `Map`/`Set` semantics.
#[derive(Debug, Clone)]
pub(super) enum Js {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Buffer(usize, Vec<u8>),
    Date(usize, f64),
    Symbol,
    Arr(usize, Vec<Js>),
    Obj(usize, Vec<(String, Js)>),
    /// `Map` and `Set` serialize as `{}`; only their identity matters.
    Map(usize),
    Set(usize),
    /// A reference to an anchored collection that is still being converted (a cyclic object
    /// in JavaScript, which `JSON.stringify` rejects).
    Cycle(usize),
}

impl Js {
    fn identity(&self) -> Option<usize> {
        match self {
            Js::Buffer(id, _)
            | Js::Date(id, _)
            | Js::Arr(id, _)
            | Js::Obj(id, _)
            | Js::Map(id)
            | Js::Set(id)
            | Js::Cycle(id) => Some(*id),
            _ => None,
        }
    }

    /// SameValueZero (`Map#has`, `Set#add`).
    fn same_value_zero(&self, other: &Js) -> bool {
        match (self, other) {
            (Js::Undefined, Js::Undefined) | (Js::Null, Js::Null) => true,
            (Js::Bool(a), Js::Bool(b)) => a == b,
            (Js::Num(a), Js::Num(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Js::Str(a), Js::Str(b)) => a == b,
            _ => match (self.identity(), other.identity()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
        }
    }
}

struct AnchorData {
    alias_count: i64,
    count: i64,
    res: Option<Js>,
}

pub(super) struct ToJs<'a> {
    nodes: &'a [Node],
    anchors: HashMap<NodeId, AnchorData>,
    /// Aliases and anchored nodes in document order (`aliasResolveCache`).
    order: Vec<NodeId>,
    next_id: usize,
    max_alias_count: i64,
}

impl<'a> ToJs<'a> {
    pub(super) fn new(nodes: &'a [Node], root: Option<NodeId>) -> Self {
        let mut order = Vec::new();
        if let Some(root) = root {
            visit(nodes, root, &mut order);
        }
        ToJs { nodes, anchors: HashMap::new(), order, next_id: 0, max_alias_count: 100 }
    }

    fn new_id(&mut self) -> usize {
        self.next_id += 1;
        self.next_id
    }

    /// `toJS(value, arg, ctx)`.
    pub(super) fn convert(&mut self, node: Option<NodeId>) -> Result<Js, String> {
        let Some(id) = node else { return Ok(Js::Null) };
        let n = &self.nodes[id];
        let has_anchor = !matches!(n.kind, Kind::Alias { .. }) && n.anchor.as_deref().is_some_and(|a| !a.is_empty());
        if !has_anchor {
            return self.node_to_js(id);
        }
        self.anchors.insert(id, AnchorData { alias_count: 0, count: 1, res: None });
        let res = self.node_to_js(id)?;
        if let Some(data) = self.anchors.get_mut(&id) {
            data.res = Some(res.clone());
        }
        Ok(res)
    }

    fn scalar_js(&mut self, value: &SVal) -> Js {
        match value {
            SVal::Null => Js::Null,
            SVal::Bool(b) => Js::Bool(*b),
            SVal::Num(n) => Js::Num(*n),
            SVal::Str(s) => Js::Str(s.clone()),
            SVal::Buffer(b) => Js::Buffer(self.new_id(), b.clone()),
            SVal::Date(d) => Js::Date(self.new_id(), *d),
            SVal::Symbol => Js::Symbol,
        }
    }

    fn node_to_js(&mut self, id: NodeId) -> Result<Js, String> {
        let nodes = self.nodes;
        match &nodes[id].kind {
            Kind::Scalar { value, .. } => Ok(self.scalar_js(value)),
            Kind::Alias { source } => self.alias_to_json(id, source),
            Kind::Map { items, class, .. } => {
                let obj_id = self.new_id();
                self.mark_created(id, obj_id, *class == MapClass::Set);
                if *class == MapClass::Set {
                    let mut set: Vec<Js> = Vec::new();
                    for pair in items {
                        if is_merge_key(nodes, pair.key) {
                            // addMergeToJSMap into a Set adds the merged keys
                            let mut target = JsTarget::Set(&mut set);
                            self.add_merge(&mut target, pair.value)?;
                            continue;
                        }
                        let key = self.convert(Some(pair.key))?;
                        if !set.iter().any(|k| k.same_value_zero(&key)) {
                            set.push(key);
                        }
                    }
                    return Ok(Js::Set(obj_id));
                }
                let mut obj: Vec<(String, Js)> = Vec::new();
                for pair in items {
                    self.add_pair_to_obj(&mut obj, pair.key, pair.value)?;
                }
                Ok(Js::Obj(obj_id, obj))
            }
            Kind::Seq { items, class, .. } => {
                let obj_id = self.new_id();
                self.mark_created(id, obj_id, false);
                if *class == SeqClass::OMap {
                    let mut map: Vec<(Js, Js)> = Vec::new();
                    for item in items {
                        let (key, value) = match item {
                            SeqItem::Pair(p) => {
                                let key = self.convert(Some(p.key))?;
                                let value = self.convert(p.value)?;
                                (key, value)
                            }
                            SeqItem::Node(n) => (self.convert(Some(*n))?, Js::Undefined),
                        };
                        if map.iter().any(|(k, _)| k.same_value_zero(&key)) {
                            return Err("Ordered maps must not include duplicate keys".to_string());
                        }
                        map.push((key, value));
                    }
                    return Ok(Js::Map(obj_id));
                }
                let mut arr = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        SeqItem::Node(n) => arr.push(self.convert(Some(*n))?),
                        SeqItem::Pair(p) => {
                            // Pair#toJSON
                            let pid = self.new_id();
                            let mut obj = Vec::new();
                            self.add_pair_to_obj(&mut obj, p.key, p.value)?;
                            arr.push(Js::Obj(pid, obj));
                        }
                    }
                }
                Ok(Js::Arr(obj_id, arr))
            }
        }
    }

    /// `ctx.onCreate(res)`: an anchored collection is referenced before it is complete.
    fn mark_created(&mut self, node: NodeId, obj_id: usize, _set: bool) {
        if let Some(data) = self.anchors.get_mut(&node)
            && data.res.is_none()
        {
            data.res = Some(Js::Cycle(obj_id));
        }
    }

    /// `Alias#resolve(doc)`: the last anchored node before this alias in document order.
    fn resolve_alias(&self, alias: NodeId, source: &str) -> Option<NodeId> {
        let mut found = None;
        for &node in &self.order {
            if node == alias {
                break;
            }
            if self.nodes[node].anchor.as_deref() == Some(source) {
                found = Some(node);
            }
        }
        found
    }

    fn alias_to_json(&mut self, alias: NodeId, source: &str) -> Result<Js, String> {
        let Some(target) = self.resolve_alias(alias, source) else {
            return Err(format!(
                "Unresolved alias (the anchor must be set before the alias): {source}"
            ));
        };
        if !self.anchors.contains_key(&target) {
            // Resolve anchors for Node.prototype.toJS()
            self.convert(Some(target))?;
        }
        if self.anchors.get(&target).and_then(|d| d.res.as_ref()).is_none() {
            return Err("This should not happen: Alias anchor was not resolved?".to_string());
        }
        if self.max_alias_count >= 0 {
            let needs_count = {
                let data = self.anchors.get_mut(&target).unwrap();
                data.count += 1;
                data.alias_count == 0
            };
            if needs_count {
                let c = self.get_alias_count(Some(target));
                self.anchors.get_mut(&target).unwrap().alias_count = c;
            }
            let data = &self.anchors[&target];
            if data.count * data.alias_count > self.max_alias_count {
                return Err("Excessive alias count indicates a resource exhaustion attack".to_string());
            }
        }
        Ok(self.anchors[&target].res.clone().unwrap())
    }

    fn get_alias_count(&self, node: Option<NodeId>) -> i64 {
        let Some(id) = node else { return 1 };
        match &self.nodes[id].kind {
            Kind::Alias { source } => {
                let target = self.resolve_alias(id, source);
                match target.and_then(|t| self.anchors.get(&t)) {
                    Some(a) => a.count * a.alias_count,
                    None => 0,
                }
            }
            Kind::Map { items, .. } => items
                .iter()
                .map(|p| self.get_alias_count(Some(p.key)).max(self.get_alias_count(p.value)))
                .fold(0, i64::max),
            Kind::Seq { items, .. } => items
                .iter()
                .map(|it| match it {
                    SeqItem::Node(n) => self.get_alias_count(Some(*n)),
                    SeqItem::Pair(p) => self.get_alias_count(Some(p.key)).max(self.get_alias_count(p.value)),
                })
                .fold(0, i64::max),
            Kind::Scalar { .. } => 1,
        }
    }

    /// `addPairToJSMap(ctx, map, pair)` for a plain object.
    fn add_pair_to_obj(&mut self, obj: &mut Vec<(String, Js)>, key: NodeId, value: Option<NodeId>) -> Result<(), String> {
        if is_merge_key(self.nodes, key) {
            let mut target = JsTarget::Obj(obj);
            return self.add_merge(&mut target, value);
        }
        let js_key = self.convert(Some(key))?;
        let string_key = self.stringify_key(key, &js_key);
        let js_value = self.convert(value)?;
        obj_set(obj, string_key, js_value);
        Ok(())
    }

    fn stringify_key(&self, key: NodeId, js_key: &Js) -> String {
        match js_key {
            Js::Null => String::new(),
            Js::Undefined => "undefined".to_string(),
            Js::Bool(b) => b.to_string(),
            Js::Num(n) => pi_ai::utils::js_json::number_to_string(*n),
            Js::Str(s) => s.clone(),
            Js::Symbol => "Symbol(<<)".to_string(),
            // Keys with collection values are stringified as YAML (approximation of
            // `key.toString(strCtx)` in flow context).
            _ => flow_string(self.nodes, key),
        }
    }

    /// `addMergeToJSMap(ctx, map, value)`.
    fn add_merge(&mut self, target: &mut JsTarget<'_>, value: Option<NodeId>) -> Result<(), String> {
        let source = self.resolve_alias_value(value);
        match source.map(|s| &self.nodes[s].kind) {
            Some(Kind::Seq { items, .. }) => {
                let items = items.clone();
                for it in items {
                    let n = match it {
                        SeqItem::Node(n) => Some(n),
                        SeqItem::Pair(_) => None,
                    };
                    self.merge_value(target, n)?;
                }
                Ok(())
            }
            _ => self.merge_value(target, source),
        }
    }

    fn resolve_alias_value(&self, value: Option<NodeId>) -> Option<NodeId> {
        let id = value?;
        match &self.nodes[id].kind {
            Kind::Alias { source } => self.resolve_alias(id, source),
            _ => Some(id),
        }
    }

    fn merge_value(&mut self, target: &mut JsTarget<'_>, value: Option<NodeId>) -> Result<(), String> {
        let source = self.resolve_alias_value(value);
        let Some(source) = source else {
            return Err("Merge sources must be maps or map aliases".to_string());
        };
        let Kind::Map { items, .. } = &self.nodes[source].kind else {
            return Err("Merge sources must be maps or map aliases".to_string());
        };
        // source.toJSON(null, ctx, Map)
        let items = items.clone();
        let mut src_map: Vec<(Js, Js)> = Vec::new();
        for pair in &items {
            if is_merge_key(self.nodes, pair.key) {
                let mut inner = JsTarget::Map(&mut src_map);
                self.add_merge(&mut inner, pair.value)?;
                continue;
            }
            let key = self.convert(Some(pair.key))?;
            let value = self.convert(pair.value)?;
            match src_map.iter_mut().find(|(k, _)| k.same_value_zero(&key)) {
                Some(entry) => entry.1 = value,
                None => src_map.push((key, value)),
            }
        }
        for (key, value) in src_map {
            match target {
                JsTarget::Map(map) => {
                    if !map.iter().any(|(k, _)| k.same_value_zero(&key)) {
                        map.push((key, value));
                    }
                }
                JsTarget::Set(set) => {
                    if !set.iter().any(|k| k.same_value_zero(&key)) {
                        set.push(key);
                    }
                }
                JsTarget::Obj(obj) => {
                    let k = js_to_property_key(&key);
                    if !obj.iter().any(|(existing, _)| *existing == k) {
                        obj.push((k, value));
                    }
                }
            }
        }
        Ok(())
    }
}

enum JsTarget<'a> {
    Obj(&'a mut Vec<(String, Js)>),
    Map(&'a mut Vec<(Js, Js)>),
    Set(&'a mut Vec<Js>),
}

fn is_merge_key(nodes: &[Node], key: NodeId) -> bool {
    matches!(nodes[key].kind, Kind::Scalar { merge: true, .. })
}

/// `map[key] = value` / `Object.defineProperty` on an existing key: the position is kept.
fn obj_set(obj: &mut Vec<(String, Js)>, key: String, value: Js) {
    match obj.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) => entry.1 = value,
        None => obj.push((key, value)),
    }
}

/// `ToPropertyKey(value)` (`String(value)` for objects).
fn js_to_property_key(v: &Js) -> String {
    match v {
        Js::Undefined => "undefined".to_string(),
        Js::Null => "null".to_string(),
        Js::Bool(b) => b.to_string(),
        Js::Num(n) => pi_ai::utils::js_json::number_to_string(*n),
        Js::Str(s) => s.clone(),
        Js::Symbol => "Symbol(<<)".to_string(),
        Js::Buffer(_, b) => String::from_utf8_lossy(b).into_owned(),
        Js::Date(..) => "Invalid Date".to_string(),
        Js::Arr(_, items) => items
            .iter()
            .map(|i| match i {
                Js::Undefined | Js::Null => String::new(),
                other => js_to_property_key(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Js::Obj(..) | Js::Cycle(_) => "[object Object]".to_string(),
        Js::Map(_) => "[object Map]".to_string(),
        Js::Set(_) => "[object Set]".to_string(),
    }
}

/// Pre-order traversal collecting aliases and anchored nodes (`visit(doc, { Node })`).
fn visit(nodes: &[Node], id: NodeId, out: &mut Vec<NodeId>) {
    let n = &nodes[id];
    let anchored = !matches!(n.kind, Kind::Alias { .. }) && n.anchor.as_deref().is_some_and(|a| !a.is_empty());
    if matches!(n.kind, Kind::Alias { .. }) || anchored {
        out.push(id);
    }
    match &n.kind {
        Kind::Map { items, .. } => {
            for p in items {
                visit(nodes, p.key, out);
                if let Some(v) = p.value {
                    visit(nodes, v, out);
                }
            }
        }
        Kind::Seq { items, .. } => {
            for it in items {
                match it {
                    SeqItem::Node(n) => visit(nodes, *n, out),
                    SeqItem::Pair(p) => {
                        visit(nodes, p.key, out);
                        if let Some(v) = p.value {
                            visit(nodes, v, out);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Approximate YAML flow stringification of a key node (`strCtx.inFlow`).
fn flow_string(nodes: &[Node], id: NodeId) -> String {
    let n = &nodes[id];
    let body = match &n.kind {
        Kind::Alias { source } => format!("*{source}"),
        Kind::Scalar { value, ty, source, .. } => match value {
            SVal::Null => {
                if matches!(source.as_str(), "" | "~" | "null" | "Null" | "NULL") {
                    source.clone()
                } else {
                    "null".to_string()
                }
            }
            SVal::Bool(b) => b.to_string(),
            SVal::Num(x) => {
                if x.is_nan() {
                    ".nan".to_string()
                } else if x.is_infinite() {
                    if *x < 0.0 { "-.inf".to_string() } else { ".inf".to_string() }
                } else {
                    pi_ai::utils::js_json::number_to_string(*x)
                }
            }
            SVal::Str(s) => {
                let plain = !s.is_empty()
                    && !s.starts_with([' ', '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
                    && !s.ends_with(' ')
                    && !s.contains(": ")
                    && !s.contains(" #")
                    && !s.contains(['\n', ',', '[', ']', '{', '}'])
                    && *ty != Some(ScalarType::QuoteDouble);
                if plain { s.clone() } else { pi_ai::utils::js_json::quote(s) }
            }
            other => super::compose::sval_to_string(other),
        },
        Kind::Map { items, .. } => {
            if items.is_empty() {
                "{}".to_string()
            } else {
                let parts: Vec<String> = items
                    .iter()
                    .map(|p| flow_pair(nodes, p.key, p.value))
                    .collect();
                format!("{{ {} }}", parts.join(", "))
            }
        }
        Kind::Seq { items, .. } => {
            if items.is_empty() {
                "[]".to_string()
            } else {
                let parts: Vec<String> = items
                    .iter()
                    .map(|it| match it {
                        SeqItem::Node(n) => flow_string(nodes, *n),
                        SeqItem::Pair(p) => flow_pair(nodes, p.key, p.value),
                    })
                    .collect();
                format!("[ {} ]", parts.join(", "))
            }
        }
    };
    match &n.anchor {
        Some(a) if !matches!(n.kind, Kind::Alias { .. }) => format!("&{a} {body}"),
        _ => body,
    }
}

fn flow_pair(nodes: &[Node], key: NodeId, value: Option<NodeId>) -> String {
    let key = flow_string(nodes, key);
    match value.map(|v| flow_string(nodes, v)) {
        Some(v) if !v.is_empty() => format!("{key}: {v}"),
        Some(_) => format!("{key}:"),
        None => key,
    }
}

fn is_array_index(key: &str) -> bool {
    if key == "0" {
        return true;
    }
    !key.is_empty()
        && !key.starts_with('0')
        && key.len() <= 10
        && key.bytes().all(|b| b.is_ascii_digit())
        && key.parse::<u64>().is_ok_and(|n| n < u32::MAX as u64)
}

fn number_value(n: f64) -> Value {
    if !n.is_finite() {
        return Value::Null;
    }
    if n == n.trunc() && n.abs() < 9_007_199_254_740_992.0 {
        return Value::Number(Number::from(n as i64));
    }
    Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
}

/// `JSON.parse(JSON.stringify(value))`; `None` for `undefined`/symbols.
pub(super) fn to_json_value(js: &Js) -> Option<Value> {
    Some(match js {
        Js::Undefined | Js::Symbol => return None,
        Js::Null | Js::Cycle(_) => Value::Null,
        Js::Bool(b) => Value::Bool(*b),
        Js::Num(n) => number_value(*n),
        Js::Str(s) => Value::String(s.clone()),
        Js::Buffer(_, data) => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("Buffer".into()));
            m.insert("data".into(), Value::Array(data.iter().map(|b| Value::from(*b)).collect()));
            Value::Object(m)
        }
        Js::Date(_, t) => match iso_string(*t) {
            Some(s) => Value::String(s),
            None => Value::Null,
        },
        Js::Arr(_, items) => Value::Array(items.iter().map(|i| to_json_value(i).unwrap_or(Value::Null)).collect()),
        Js::Obj(_, entries) => {
            let mut index_keys: Vec<&(String, Js)> = entries.iter().filter(|(k, _)| is_array_index(k)).collect();
            index_keys.sort_by_key(|(k, _)| k.parse::<u64>().unwrap_or(0));
            let mut m = Map::new();
            for (k, v) in index_keys.into_iter().chain(entries.iter().filter(|(k, _)| !is_array_index(k))) {
                if let Some(v) = to_json_value(v) {
                    m.insert(k.clone(), v);
                }
            }
            Value::Object(m)
        }
        Js::Map(_) | Js::Set(_) => Value::Object(Map::new()),
    })
}

/// `Date.prototype.toISOString` (`None` for an invalid date).
fn iso_string(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let ms = t as i64;
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    Some(format!("{year}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z"))
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
