//! Persisted Chord operation tuples and per-stream path dictionaries.
//!
//! Operations are exact, but their shape is not canonical. Payload ownership is
//! expressed with Rust ownership rather than JavaScript's mutable aliases.

mod diff;
mod shared;
mod tracker;
pub use diff::diff_revisions;
pub use shared::{SharedJson, WeakSharedJson};
pub use tracker::{Change, Prepared, Tracker, track};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::json::{JsonString, Value, stringify};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! json { ($($tokens:tt)+) => { Value::from(serde_json::json!($($tokens)+)) }; }

pub const RESERVED_SEGMENTS: [&str; 3] = ["__proto__", "constructor", "prototype"];

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum Seg {
    Key(JsonString),
    Index(usize),
}

impl From<&str> for Seg {
    fn from(value: &str) -> Self {
        Self::Key(value.into())
    }
}

impl From<usize> for Seg {
    fn from(value: usize) -> Self {
        Self::Index(value)
    }
}

pub type Path = Vec<Seg>;

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Replace(Value),
    Set(Path, Value),
    Delete(Path),
    Append(Path, JsonString),
    Truncate(Path, usize),
    Splice(Path, usize, usize, Vec<Value>),
    Move(Path, Vec<usize>),
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Type(String),
    #[error("unsafe path segment: {0}")]
    UnsafePath(String),
    #[error("unresolvable path: {0}")]
    Path(Value),
}

type Result<T> = std::result::Result<T, Error>;

fn shape(message: &str) -> Error {
    Error::Type(message.into())
}

fn integer(value: &Value) -> Option<usize> {
    let number = value.as_f64()?;
    (number.is_finite() && number >= 0.0 && number.fract() == 0.0 && number < usize::MAX as f64)
        .then_some(number as usize)
}

pub fn assert_safe_path(path: &[Seg]) -> Result<()> {
    for segment in path {
        if let Seg::Key(key) = segment
            && RESERVED_SEGMENTS
                .iter()
                .any(|reserved| key.equals_str(reserved))
        {
            return Err(Error::UnsafePath(key.to_string_lossy()));
        }
    }
    Ok(())
}

fn parse_path(value: &Value) -> Result<Path> {
    let values = value
        .as_array()
        .ok_or_else(|| shape("path is not an array"))?;
    let mut path = Vec::with_capacity(values.len());
    for value in values {
        path.push(if let Some(key) = value.as_string() {
            Seg::Key(key.clone())
        } else if let Some(index) = integer(value) {
            Seg::Index(index)
        } else {
            return Err(Error::UnsafePath(stringify(value)));
        });
    }
    assert_safe_path(&path)?;
    Ok(path)
}

fn permutation(value: &Value) -> Result<Vec<usize>> {
    let values = value
        .as_array()
        .ok_or_else(|| shape("m permutation is not an array"))?;
    let mut seen = vec![false; values.len()];
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        let index = integer(value)
            .filter(|i| *i < values.len())
            .ok_or_else(|| shape("m permutation is not a bijection"))?;
        if seen[index] {
            return Err(shape("m permutation is not a bijection"));
        }
        seen[index] = true;
        result.push(index);
    }
    Ok(result)
}

impl Op {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Replace(value) => Value::Array(vec![Value::from("r"), value.clone()].into()),
            Self::Set(path, value) => {
                Value::Array(vec![Value::from("s"), path_value(path), value.clone()].into())
            }
            Self::Delete(path) => Value::Array(vec![Value::from("d"), path_value(path)].into()),
            Self::Append(path, value) => Value::Array(
                vec![
                    Value::from("a"),
                    path_value(path),
                    Value::String(value.clone()),
                ]
                .into(),
            ),
            Self::Truncate(path, count) => {
                Value::Array(vec![Value::from("t"), path_value(path), Value::from(*count)].into())
            }
            Self::Splice(path, index, remove, items) => Value::Array(
                vec![
                    Value::from("p"),
                    path_value(path),
                    Value::from(*index),
                    Value::from(*remove),
                    Value::Array(items.clone().into()),
                ]
                .into(),
            ),
            Self::Move(path, order) => Value::Array(
                vec![
                    Value::from("m"),
                    path_value(path),
                    Value::Array(
                        order
                            .iter()
                            .map(|i| Value::from(*i))
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                ]
                .into(),
            ),
        }
    }

    pub fn from_value(value: &Value) -> Result<Self> {
        let tuple = value
            .as_array()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| shape("op is not a tuple"))?;
        let verb_text = tuple[0].to_utf8().unwrap_or_default();
        let verb = verb_text.as_str();
        let len = tuple.len();
        let path = |nonempty: bool| -> Result<Path> {
            let path = parse_path(&tuple[1])?;
            if nonempty && path.is_empty() {
                return Err(shape("path is empty"));
            }
            Ok(path)
        };
        match verb {
            "r" if len == 2 => Ok(Self::Replace(tuple[1].clone())),
            "s" if len == 3 => Ok(Self::Set(path(true)?, tuple[2].clone())),
            "d" if len == 2 => Ok(Self::Delete(path(true)?)),
            "a" if len == 3 && tuple[2].is_string() => Ok(Self::Append(
                path(true)?,
                tuple[2].as_string().unwrap().clone(),
            )),
            "t" if len == 3 && integer(&tuple[2]).is_some() => {
                Ok(Self::Truncate(path(true)?, integer(&tuple[2]).unwrap()))
            }
            "p" if len == 5 => {
                let path = path(false)?;
                let index = integer(&tuple[2]).ok_or_else(|| shape("p index"))?;
                let remove = integer(&tuple[3]).ok_or_else(|| shape("p remove"))?;
                let items = tuple[4].as_array().ok_or_else(|| shape("p items"))?;
                Ok(Self::Splice(path, index, remove, items.clone()))
            }
            "m" if len == 3 => Ok(Self::Move(path(false)?, permutation(&tuple[2])?)),
            "r" | "s" | "d" | "p" | "m" => Err(shape(&format!("{verb} arity"))),
            "a" | "t" => Err(shape(&format!("{verb} shape"))),
            _ => Err(shape(&format!(
                "unknown op verb: {}",
                tuple[0].to_utf8().unwrap_or_else(|| stringify(&tuple[0]))
            ))),
        }
    }

    pub fn path(&self) -> Option<&[Seg]> {
        match self {
            Self::Replace(_) => None,
            Self::Set(p, _)
            | Self::Delete(p)
            | Self::Append(p, _)
            | Self::Truncate(p, _)
            | Self::Splice(p, ..)
            | Self::Move(p, _) => Some(p),
        }
    }

    pub fn validate(&self) -> Result<()> {
        Self::from_value(&self.to_value()).map(|_| ())
    }
}

impl Serialize for Op {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Op {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::from_value(&Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

pub fn is_replace(op: &Op) -> bool {
    matches!(op, Op::Replace(_))
}
pub fn is_base(ops: &[Op]) -> bool {
    ops.first().is_some_and(is_replace)
}

fn path_error(path: &[Seg]) -> Error {
    Error::Path(path_value(path))
}
fn key_string(segment: &Seg) -> JsonString {
    match segment {
        Seg::Key(key) => key.clone(),
        Seg::Index(index) => index.to_string().into(),
    }
}

/// Whether a string names an own property of an array of `len` elements.
fn array_owns_key(key: &JsonString, len: usize) -> bool {
    let key = key.to_string_lossy();
    key == "length"
        || key
            .parse::<usize>()
            .is_ok_and(|index| index < len && index.to_string() == key)
}

/// `own_first` selects the immutable replay's walk order, which reports a missing
/// own property as unresolvable before rejecting a non-index segment of an array.
fn resolve_mut<'a>(
    mut value: &'a mut Value,
    path: &[Seg],
    own_first: bool,
) -> Result<&'a mut Value> {
    for segment in path {
        value = match value {
            Value::Object(map) => Arc::make_mut(map)
                .get_mut(&key_string(segment))
                .ok_or_else(|| path_error(path))?,
            Value::Array(items) => match segment {
                Seg::Index(index) => Arc::make_mut(items)
                    .get_mut(*index)
                    .ok_or_else(|| path_error(path))?,
                Seg::Key(key) if own_first && !array_owns_key(key, items.len()) => {
                    return Err(path_error(path));
                }
                Seg::Key(key) => return Err(Error::UnsafePath(key.to_string_lossy())),
            },
            _ => return Err(path_error(path)),
        };
    }
    if !value.is_object() && !value.is_array() {
        return Err(path_error(path));
    }
    Ok(value)
}

/// Apply an owned batch to an exclusively owned mutable replica. Errors may leave
/// earlier operations applied, as in Chord. Discard a failed replica and recover at a base.
pub fn apply(root: Value, ops: &[Op]) -> Result<Value> {
    apply_with(root, ops, false)
}

fn apply_with(mut root: Value, ops: &[Op], own_first: bool) -> Result<Value> {
    for op in ops {
        op.validate()?;
        if let Op::Replace(value) = op {
            root = value.clone();
            continue;
        }
        let path = op.path().unwrap();
        if let Op::Splice(_, index, remove, items) = op {
            let target = resolve_mut(&mut root, path, own_first)?
                .as_array_mut()
                .ok_or_else(|| path_error(path))?;
            let start = (*index).min(target.len());
            let end = start.saturating_add(*remove).min(target.len());
            target.splice(start..end, items.iter().cloned());
            continue;
        }
        if let Op::Move(_, order) = op {
            let target = resolve_mut(&mut root, path, own_first)?
                .as_array_mut()
                .ok_or_else(|| path_error(path))?;
            if target.len() != order.len() {
                return Err(path_error(path));
            }
            let previous = target.clone();
            for (index, source) in order.iter().enumerate() {
                target[index] = previous[*source].clone();
            }
            continue;
        }
        let (key, parent_path) = path.split_last().ok_or_else(|| shape("path is empty"))?;
        let parent = resolve_mut(&mut root, parent_path, own_first)?;
        if let Value::Array(items) = parent {
            let items = Arc::make_mut(items);
            let Seg::Index(index) = key else {
                return Err(Error::UnsafePath(key_string(key).to_string_lossy()));
            };
            if *index > items.len() {
                return Err(Error::UnsafePath(index.to_string()));
            }
            match op {
                Op::Set(_, value) if *index == items.len() => items.push(value.clone()),
                Op::Set(_, value) => items[*index] = value.clone(),
                Op::Delete(_) if *index < items.len() => {
                    items.remove(*index);
                }
                Op::Delete(_) => return Err(path_error(path)),
                _ => update_string(items.get_mut(*index), op, path)?,
            }
        } else if let Value::Object(map) = parent {
            let map = Arc::make_mut(map);
            let key = key_string(key);
            match op {
                Op::Set(_, value) => {
                    map.insert(key, value.clone());
                }
                Op::Delete(_) => {
                    map.shift_remove(&key);
                }
                _ => update_string(map.get_mut(&key), op, path)?,
            }
        }
    }
    Ok(root)
}

fn update_string(value: Option<&mut Value>, op: &Op, path: &[Seg]) -> Result<()> {
    let Some(Value::String(text)) = value else {
        return Err(path_error(path));
    };
    match op {
        Op::Append(_, suffix) => text.append(suffix),
        Op::Truncate(_, count) => {
            // Chord counts JavaScript UTF-16 code units, not UTF-8 bytes or scalars.
            text.truncate_front(*count);
        }
        _ => unreachable!("non-string operation dispatched to string updater"),
    }
    Ok(())
}

pub fn apply_immutable(target: Option<&Value>, ops: &[Op]) -> Result<Value> {
    apply_with(target.cloned().unwrap_or(Value::Null), ops, true)
}

pub fn apply_immutable_batches<'a>(
    target: Option<&Value>,
    batches: impl IntoIterator<Item = &'a [Op]>,
) -> Result<Value> {
    let mut value = target.cloned().unwrap_or(Value::Null);
    for ops in batches {
        value = apply_with(value, ops, true)?;
    }
    Ok(value)
}

/// Longest suffix/prefix match found within Chord's bounded candidate budget.
/// Lengths and positions are UTF-16 code units.
pub fn overlap(a: &str, b: &str, scan: usize, probe: usize, max_candidates: usize) -> usize {
    let a: Vec<_> = a.encode_utf16().collect();
    let b: Vec<_> = b.encode_utf16().collect();
    overlap_units(&a, &b, scan, probe, max_candidates)
}
/// Lossless counterpart for strings that contain arbitrary UTF16 code units.
pub fn overlap_units(
    a: &[u16],
    b: &[u16],
    scan: usize,
    probe: usize,
    max_candidates: usize,
) -> usize {
    if a.is_empty() || b.is_empty() || scan == 0 {
        return 0;
    }
    let tail = &a[a.len().saturating_sub(scan)..];
    for h in [probe.min(b.len()), 1] {
        let mut tried = 0;
        for k in 0..=tail.len() {
            if k + h > tail.len() || tail[k..k + h] != b[..h] {
                continue;
            }
            tried += 1;
            if tried > max_candidates {
                break;
            }
            let n = tail.len() - k;
            if n <= b.len() && tail[k..] == b[..n] {
                return n;
            }
        }
        if h == 1 {
            break;
        }
    }
    0
}

#[derive(Default)]
pub struct Encoder {
    seen: HashSet<Path>,
    ids: HashMap<Path, usize>,
    next_id: usize,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(&mut self, ops: &[Op]) -> Result<Vec<Value>> {
        let mut previous: Option<Path> = None;
        let mut out = Vec::new();
        for op in ops {
            op.validate()?;
            let mut tuple = op.to_value().as_array().unwrap().clone();
            let Some(path) = op.path() else {
                out.push(Value::Array(tuple.into()));
                self.seen.clear();
                self.ids.clear();
                self.next_id = 0;
                previous = None;
                continue;
            };
            if previous.as_deref() == Some(path) {
                tuple.remove(1);
            } else {
                if let Some(id) = self.ids.get(path) {
                    tuple[1] = json!(id);
                } else if !self.seen.insert(path.to_vec()) {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.ids.insert(path.to_vec(), id);
                    out.push(Value::Array(
                        vec![Value::from("#"), Value::from(id), path_value(path)].into(),
                    ));
                    tuple[1] = json!(id);
                }
                previous = Some(path.to_vec());
            }
            out.push(Value::Array(tuple.into()));
        }
        Ok(out)
    }
}

#[derive(Default)]
pub struct Decoder {
    paths: HashMap<usize, Path>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(&mut self, wire: &[Value]) -> Result<Vec<Op>> {
        let mut previous: Option<Path> = None;
        let mut out = Vec::new();
        for raw in wire {
            assert_valid_wire_op(raw)?;
            let tuple = raw.as_array().unwrap();
            let verb_text = tuple[0].to_utf8().unwrap();
            let verb = verb_text.as_str();
            if verb == "#" {
                self.paths
                    .insert(integer(&tuple[1]).unwrap(), parse_path(&tuple[2])?);
                continue;
            }
            if verb == "r" {
                out.push(Op::Replace(tuple[1].clone()));
                self.paths.clear();
                previous = None;
                continue;
            }
            let short = tuple.len()
                == match verb {
                    "d" => 1,
                    "p" => 4,
                    _ => 2,
                };
            let path = if short {
                previous.clone().ok_or_else(|| Error::Path(json!([])))?
            } else {
                let path = if tuple[1].is_number() {
                    let id = integer(&tuple[1]).unwrap();
                    self.paths
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| Error::Path(tuple[1].clone()))?
                } else {
                    parse_path(&tuple[1])?
                };
                previous = Some(path.clone());
                path
            };
            if path.is_empty() && verb != "p" && verb != "m" {
                return Err(path_error(&path));
            }
            let mut expanded = vec![tuple[0].clone(), path_value(&path)];
            expanded.extend_from_slice(&tuple[if short { 1 } else { 2 }..]);
            out.push(Op::from_value(&Value::Array(expanded.into()))?);
        }
        Ok(out)
    }
}

pub fn assert_valid_wire_op(raw: &Value) -> Result<()> {
    let t = raw
        .as_array()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| shape("op is not a tuple"))?;
    let verb_text = t[0].to_utf8().unwrap_or_default();
    let verb = verb_text.as_str();
    let len = t.len();
    let check_ref = |value: &Value| -> Result<()> {
        if value.is_number() {
            integer(value).ok_or_else(|| shape("bad path id"))?;
        } else {
            parse_path(value)?;
        }
        Ok(())
    };
    let check_arity = |short: usize, full: usize| -> Result<()> {
        if len == full {
            check_ref(&t[1])
        } else if len == short {
            Ok(())
        } else {
            Err(shape(&format!("{verb} arity")))
        }
    };
    match verb {
        "r" => {
            if len == 2 {
                Ok(())
            } else {
                Err(shape("r arity"))
            }
        }
        "s" => check_arity(2, 3),
        "d" => check_arity(1, 2),
        "a" => {
            check_arity(2, 3)?;
            if t[len - 1].is_string() {
                Ok(())
            } else {
                Err(shape("a value"))
            }
        }
        "t" => {
            check_arity(2, 3)?;
            integer(&t[len - 1])
                .ok_or_else(|| shape("t count"))
                .map(|_| ())
        }
        "p" => {
            check_arity(4, 5)?;
            integer(&t[len - 3]).ok_or_else(|| shape("p index"))?;
            integer(&t[len - 2]).ok_or_else(|| shape("p remove"))?;
            if t[len - 1].is_array() {
                Ok(())
            } else {
                Err(shape("p items"))
            }
        }
        "m" => {
            check_arity(2, 3)?;
            permutation(&t[len - 1]).map(|_| ())
        }
        "#" => {
            if len != 3 || integer(&t[1]).is_none() || !t[2].is_array() {
                return Err(shape("# shape"));
            }
            parse_path(&t[2]).map(|_| ())
        }
        _ => Err(shape(&format!(
            "unknown op verb: {}",
            t[0].to_utf8().unwrap_or_else(|| stringify(&t[0]))
        ))),
    }
}

fn path_value(path: &[Seg]) -> Value {
    Value::Array(
        path.iter()
            .map(|s| match s {
                Seg::Key(k) => Value::String(k.clone()),
                Seg::Index(i) => Value::from(*i),
            })
            .collect::<Vec<_>>()
            .into(),
    )
}

impl<'de> Deserialize<'de> for Seg {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(s) => Ok(Self::Key(s)),
            value => integer(&value)
                .map(Self::Index)
                .ok_or_else(|| serde::de::Error::custom("invalid path segment")),
        }
    }
}
