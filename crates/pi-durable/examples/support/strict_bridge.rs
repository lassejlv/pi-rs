//! Preserve actual native container and operation-batch identities over JSON.
//! Identity comes from live Arc owners, never structural equality or record IDs.
use super::*;
use pi_chord::delta::Op;
use std::sync::Weak;

enum Container {
    Array(Weak<Vec<JsonValue>>),
    Object(Weak<pi_chord::json::Map>),
}
impl Container {
    fn matches(&self, value: &JsonValue) -> bool {
        match (self, value) {
            (Self::Array(held), JsonValue::Array(value)) => {
                held.upgrade().is_some_and(|held| Arc::ptr_eq(&held, value))
            }
            (Self::Object(held), JsonValue::Object(value)) => {
                held.upgrade().is_some_and(|held| Arc::ptr_eq(&held, value))
            }
            _ => false,
        }
    }
    fn alive(&self) -> bool {
        match self {
            Self::Array(value) => value.strong_count() != 0,
            Self::Object(value) => value.strong_count() != 0,
        }
    }
}
#[derive(Default)]
pub(super) struct StrictBridge {
    nodes: Mutex<Vec<(u64, Container)>>,
    ops: Mutex<Vec<(Weak<Vec<Op>>, JsonValue)>>,
}
impl Hub {
    fn strict_node(&self, value: &JsonValue) -> JsonValue {
        let container = match value {
            JsonValue::Array(value) => Container::Array(Arc::downgrade(value)),
            JsonValue::Object(value) => Container::Object(Arc::downgrade(value)),
            _ => return object([("scalar", value.clone())]),
        };
        let id = {
            let mut nodes = self.strict.nodes.lock();
            nodes.retain(|(_, value)| value.alive());
            if let Some((id, _)) = nodes.iter().find(|(_, held)| held.matches(value)) {
                *id
            } else {
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                nodes.push((id, container));
                id
            }
        };
        if let Some(values) = value.as_array() {
            object([
                ("id", encode(&id)),
                (
                    "array",
                    JsonValue::Array(
                        values
                            .iter()
                            .map(|value| self.strict_node(value))
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                ),
            ])
        } else {
            let values = value
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| {
                    JsonValue::Array(
                        vec![JsonValue::String(key.clone()), self.strict_node(value)].into(),
                    )
                })
                .collect::<Vec<_>>();
            object([
                ("id", encode(&id)),
                ("object", JsonValue::Array(values.into())),
            ])
        }
    }
    pub(super) fn native_value(&self, value: &JsonValue) -> JsonValue {
        object([("shared", self.strict_node(value))])
    }
    pub(super) fn native_ops(&self, ops: &Arc<Vec<Op>>) -> JsonValue {
        let value = {
            let mut batches = self.strict.ops.lock();
            batches.retain(|(held, _)| held.strong_count() != 0);
            if let Some((_, value)) = batches
                .iter()
                .find(|(held, _)| held.upgrade().is_some_and(|held| Arc::ptr_eq(&held, ops)))
            {
                value.clone()
            } else {
                let value =
                    JsonValue::Array(ops.iter().map(Op::to_value).collect::<Vec<_>>().into());
                batches.push((Arc::downgrade(ops), value.clone()));
                value
            }
        };
        self.native_value(&value)
    }
    pub(super) fn native_publication(&self, publication: &CommitPublication) -> JsonValue {
        let mut wire = encode(publication);
        let changes = wire
            .as_object_mut()
            .unwrap()
            .get_mut(&JsonString::from("changes"))
            .unwrap()
            .as_array_mut()
            .unwrap();
        for (wire, change) in changes.iter_mut().zip(&publication.changes) {
            if let CommitChange::Document(DocumentCommitChange::Document { value, ops, .. }) =
                change
            {
                wire.as_object_mut()
                    .unwrap()
                    .insert("nativeValue".into(), self.native_value(value));
                wire.as_object_mut()
                    .unwrap()
                    .insert("nativeOps".into(), self.native_ops(ops));
            }
        }
        wire
    }
    pub(super) fn native_storage_identities(&self, writes: &[StorageWrite]) -> JsonValue {
        let mut values = Vec::new();
        for (index, write) in writes.iter().enumerate() {
            let value = match write {
                StorageWrite::DocumentCreate { content, .. } => object([
                    ("index", encode(&index)),
                    ("value", self.native_value(content.value.value())),
                ]),
                StorageWrite::DocumentChange {
                    content: DocumentContent::Base { value, .. },
                    ..
                } => object([
                    ("index", encode(&index)),
                    ("value", self.native_value(value.value())),
                ]),
                StorageWrite::DocumentChange {
                    content: DocumentContent::Delta { ops, .. },
                    ..
                } => object([("index", encode(&index)), ("ops", self.native_ops(ops))]),
                _ => continue,
            };
            values.push(value);
        }
        JsonValue::Array(values.into())
    }
}
