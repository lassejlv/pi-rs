//! Immutable JSON revisions with copy-on-write containers and stable child identity.
use super::{Error, Op, Seg, array_owns_key, key_string, path_error};
use crate::json::{JsonString, Value};
use indexmap::IndexMap;
use serde::Serialize;
use std::sync::{Arc, Weak};

#[derive(Clone, Debug)]
pub struct SharedJson(Arc<Node>);
#[derive(Clone, Debug)]
enum Node {
    Scalar(Value),
    Array(Vec<SharedJson>),
    Object(IndexMap<JsonString, SharedJson>),
}
#[derive(Clone)]
pub struct WeakSharedJson(Weak<Node>);
impl WeakSharedJson {
    pub fn upgrade(&self) -> Option<SharedJson> {
        self.0.upgrade().map(SharedJson)
    }
}
impl SharedJson {
    pub fn array(values: impl IntoIterator<Item = Self>) -> Self {
        Self(Arc::new(Node::Array(values.into_iter().collect())))
    }
    pub fn object(values: impl IntoIterator<Item = (JsonString, Self)>) -> Self {
        Self(Arc::new(Node::Object(values.into_iter().collect())))
    }
    pub fn new(value: Value) -> Self {
        Self(Arc::new(match value {
            Value::Array(values) => Node::Array(
                Arc::unwrap_or_clone(values)
                    .into_iter()
                    .map(Self::new)
                    .collect(),
            ),
            Value::Object(values) => Node::Object(
                Arc::unwrap_or_clone(values)
                    .into_iter()
                    .map(|(key, value)| (key, Self::new(value)))
                    .collect(),
            ),
            value => Node::Scalar(value),
        }))
    }
    pub fn downgrade(&self) -> WeakSharedJson {
        WeakSharedJson(Arc::downgrade(&self.0))
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    /// JavaScript strict equality for JSON: containers by identity, primitives by value.
    pub fn same_value(&self, other: &Self) -> bool {
        match (self.0.as_ref(), other.0.as_ref()) {
            (Node::Scalar(left), Node::Scalar(right)) => left == right,
            _ => self.ptr_eq(other),
        }
    }
    pub fn same_optional(left: Option<&Self>, right: Option<&Self>) -> bool {
        match (left, right) {
            (None, None) => true,
            (Some(a), Some(b)) => a.same_value(b),
            _ => false,
        }
    }
    pub fn as_array(&self) -> Option<&[Self]> {
        if let Node::Array(values) = self.0.as_ref() {
            Some(values)
        } else {
            None
        }
    }
    pub fn as_object(&self) -> Option<&IndexMap<JsonString, Self>> {
        if let Node::Object(values) = self.0.as_ref() {
            Some(values)
        } else {
            None
        }
    }
    pub fn as_scalar(&self) -> Option<&Value> {
        if let Node::Scalar(value) = self.0.as_ref() {
            Some(value)
        } else {
            None
        }
    }
    pub fn get_key(&self, key: &str) -> Option<&Self> {
        self.as_object()?.get(&JsonString::from(key))
    }
    pub fn get(&self, path: &[Seg]) -> Option<&Self> {
        let mut current = self;
        for segment in path {
            current = match current.0.as_ref() {
                Node::Array(values) => match segment {
                    Seg::Index(index) => values.get(*index)?,
                    _ => return None,
                },
                Node::Object(values) => values.get(&key_string(segment))?,
                Node::Scalar(_) => return None,
            };
        }
        Some(current)
    }
    pub fn to_value(&self) -> Value {
        match self.0.as_ref() {
            Node::Scalar(value) => value.clone(),
            Node::Array(values) => {
                Value::Array(values.iter().map(Self::to_value).collect::<Vec<_>>().into())
            }
            Node::Object(values) => Value::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), value.to_value()))
                    .collect::<crate::json::Map>()
                    .into(),
            ),
        }
    }
    pub fn apply(&self, ops: &[Op]) -> Result<Self, Error> {
        let mut root = self.clone();
        for op in ops {
            op.validate()?;
            if let Op::Replace(value) = op {
                root = Self::new(value.clone());
                continue;
            }
            let path = op.path().expect("validated non-replacement has a path");
            if let Op::Splice(_, index, remove, items) = op {
                let Node::Array(values) = container_mut(&mut root, path)? else {
                    return Err(path_error(path));
                };
                let start = (*index).min(values.len());
                let end = start.saturating_add(*remove).min(values.len());
                values.splice(start..end, items.iter().cloned().map(Self::new));
                continue;
            }
            if let Op::Move(_, order) = op {
                let Node::Array(values) = container_mut(&mut root, path)? else {
                    return Err(path_error(path));
                };
                if values.len() != order.len() {
                    return Err(path_error(path));
                }
                let previous = values.clone();
                for (index, source) in order.iter().enumerate() {
                    values[index] = previous[*source].clone();
                }
                continue;
            }
            let (key, parent) = path
                .split_last()
                .expect("validated leaf operation has a nonempty path");
            match container_mut(&mut root, parent)? {
                Node::Array(values) => {
                    let Seg::Index(index) = key else {
                        return Err(Error::UnsafePath(key_string(key).to_string_lossy()));
                    };
                    if *index > values.len() {
                        return Err(Error::UnsafePath(index.to_string()));
                    }
                    match op {
                        Op::Set(_, value) if *index == values.len() => {
                            values.push(Self::new(value.clone()))
                        }
                        Op::Set(_, value) => values[*index] = Self::new(value.clone()),
                        Op::Delete(_) if *index < values.len() => {
                            values.remove(*index);
                        }
                        Op::Delete(_) => return Err(path_error(path)),
                        _ => update_string(values.get_mut(*index), op, path)?,
                    }
                }
                Node::Object(values) => {
                    let key = key_string(key);
                    match op {
                        Op::Set(_, value) => {
                            values.insert(key, Self::new(value.clone()));
                        }
                        Op::Delete(_) => {
                            values.shift_remove(&key);
                        }
                        _ => update_string(values.get_mut(&key), op, path)?,
                    }
                }
                Node::Scalar(_) => unreachable!("container_mut refuses a scalar"),
            }
        }
        Ok(root)
    }
}
fn container_mut<'a>(root: &'a mut SharedJson, path: &[Seg]) -> Result<&'a mut Node, Error> {
    let mut current = root;
    for segment in path {
        current = match Arc::make_mut(&mut current.0) {
            Node::Array(values) => match segment {
                Seg::Index(index) => values.get_mut(*index).ok_or_else(|| path_error(path))?,
                Seg::Key(key) if !array_owns_key(key, values.len()) => {
                    return Err(path_error(path));
                }
                Seg::Key(key) => return Err(Error::UnsafePath(key.to_string_lossy())),
            },
            Node::Object(values) => values
                .get_mut(&key_string(segment))
                .ok_or_else(|| path_error(path))?,
            Node::Scalar(_) => return Err(path_error(path)),
        };
    }
    let node = Arc::make_mut(&mut current.0);
    if matches!(node, Node::Scalar(_)) {
        return Err(path_error(path));
    }
    Ok(node)
}
fn update_string(value: Option<&mut SharedJson>, op: &Op, path: &[Seg]) -> Result<(), Error> {
    let Some(value) = value else {
        return Err(path_error(path));
    };
    let Node::Scalar(Value::String(text)) = Arc::make_mut(&mut value.0) else {
        return Err(path_error(path));
    };
    match op {
        Op::Append(_, suffix) => text.append(suffix),
        Op::Truncate(_, count) => text.truncate_front(*count),
        _ => unreachable!("validated string operation"),
    }
    Ok(())
}
impl Serialize for SharedJson {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}
