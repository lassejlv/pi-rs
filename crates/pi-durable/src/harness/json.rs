//! Leaf-wise assignment for streaming document fields.
use crate::{Error, JsonValue, Result};
use pi_chord::delta::{Op, Path, Seg};

/// Operations for assigning one field. Existing records delete missing children
/// first; growing arrays retain their existing leaves. Shrinking arrays replace.
pub fn assign_json_ops(current: Option<&JsonValue>, value: &JsonValue, path: &[Seg]) -> Vec<Op> {
    let mut out = Vec::new();
    assign(current, value, &mut path.to_vec(), &mut out);
    out
}
fn assign(current: Option<&JsonValue>, value: &JsonValue, path: &mut Path, out: &mut Vec<Op>) {
    match (current, value) {
        (Some(JsonValue::Object(a)), JsonValue::Object(b)) => {
            for (key, _) in pi_chord::json::object_entries(a) {
                if !b.contains_key(key) {
                    let mut child = path.clone();
                    child.push(Seg::Key(key.clone()));
                    out.push(Op::Delete(child));
                }
            }
            for (key, value) in pi_chord::json::object_entries(b) {
                path.push(Seg::Key(key.clone()));
                assign(a.get(key), value, path, out);
                path.pop();
            }
        }
        (Some(JsonValue::Array(a)), JsonValue::Array(b)) if a.len() <= b.len() => {
            for (index, value) in b.iter().enumerate() {
                if let Some(current) = a.get(index) {
                    path.push(Seg::Index(index));
                    assign(Some(current), value, path, out);
                    path.pop();
                } else {
                    out.push(Op::Splice(path.clone(), index, 0, vec![value.clone()]));
                }
            }
        }
        (Some(current), value) if current == value => {}
        (Some(current @ JsonValue::String(_)), value @ JsonValue::String(_)) => {
            // Use Chord's same string append/overlap decisions at this leaf.
            let before = JsonValue::Object(
                indexmap::IndexMap::from_iter([("leaf".into(), current.clone())]).into(),
            );
            let after = JsonValue::Object(
                indexmap::IndexMap::from_iter([("leaf".into(), value.clone())]).into(),
            );
            for op in pi_chord::delta::diff_revisions(&before, &after) {
                out.push(match op {
                    Op::Append(_, text) => Op::Append(path.clone(), text),
                    Op::Truncate(_, count) => Op::Truncate(path.clone(), count),
                    _ => Op::Set(path.clone(), value.clone()),
                });
            }
        }
        _ => out.push(Op::Set(path.clone(), value.clone())),
    }
}
pub fn assign_json(target: &mut JsonValue, key: Seg, value: JsonValue) -> Result<Vec<Op>> {
    let current = match (&*target, &key) {
        (JsonValue::Object(object), Seg::Key(key)) => object.get(key),
        (JsonValue::Array(array), Seg::Index(index)) => array.get(*index),
        _ => return Err(Error::message("Expected a JSON container")),
    };
    let ops = assign_json_ops(current, &value, &[key]);
    *target = pi_chord::delta::apply(target.clone(), &ops)
        .map_err(|error| Error::message(error.to_string()))?;
    Ok(ops)
}
