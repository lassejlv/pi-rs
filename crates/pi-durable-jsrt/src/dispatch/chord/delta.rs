//! `delta.*`: Chord operation validation, path safety, string overlap, the wire codec and
//! `diffRevisions` over `pi_chord::delta` (`js/internal/chord/delta/*.js`).
//!
//! Operation payloads never cross: the glue sends each op's *shape* (payload slots replaced by
//! `null` / `[]`) and puts its own payload objects back, so payload identity is the caller's, as
//! upstream. `delta.diff` receives both revisions as one identity graph (each JS container once),
//! so containers shared by reference become shared `Arc`s, which is what the native diff aligns
//! on; payloads come back as references to graph nodes.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use pi_chord::delta::{self, Decoder, Encoder, Error as DeltaError, Op, RESERVED_SEGMENTS, Seg};
use pi_json::strict::{self, Map, Value};

use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Methods};
use crate::value::HostValue;

/// A `pi_chord::delta::Error` as the upstream class: `TypeError`, `UnsafePathError` or `PathError`.
pub fn delta_error(error: DeltaError) -> HostError {
    match error {
        DeltaError::Type(message) => HostError::type_error(message),
        DeltaError::UnsafePath(segment) => {
            let reserved = RESERVED_SEGMENTS.contains(&segment.as_str());
            let value = match segment.parse::<f64>() {
                Ok(n) if !reserved => HostValue::from(n),
                _ => HostValue::string(&segment),
            };
            HostError::new(
                ErrorClass::UnsafePathError,
                format!("unsafe path segment: {segment}"),
            )
            .with_prop("segment", value)
        }
        DeltaError::Path(path) => HostError::new(
            ErrorClass::PathError,
            format!("unresolvable path: {}", strict::stringify(&path)),
        )
        .with_prop("path", HostValue::from_strict(&path)),
    }
}

fn op_arg(args: &Args, i: usize) -> HostResult<Value> {
    args.get(i).to_strict()
}

fn ops_arg(args: &Args, i: usize) -> HostResult<Vec<Op>> {
    let value = op_arg(args, i)?;
    let items = value
        .as_array()
        .ok_or_else(|| HostError::type_error("ops must be an array"))?;
    items
        .iter()
        .map(|op| Op::from_value(op).map_err(delta_error))
        .collect()
}

fn op_to_host(op: &Op) -> HostValue {
    HostValue::from_strict(&op.to_value())
}

// ---------------------------------------------------------------------------
// Identity graph (`delta.diff`)
// ---------------------------------------------------------------------------

struct Graph<'a> {
    nodes: &'a [HostValue],
    built: Vec<Option<Value>>,
    building: Vec<bool>,
    /// `Arc` pointer of each built container → its node index.
    index: HashMap<usize, usize>,
}

fn container_ptr(value: &Value) -> Option<usize> {
    match value {
        Value::Array(a) => Some(Arc::as_ptr(a) as *const () as usize),
        Value::Object(o) => Some(Arc::as_ptr(o) as *const () as usize),
        _ => None,
    }
}

impl Graph<'_> {
    fn child(&mut self, code: &HostValue) -> HostResult<Value> {
        match code {
            HostValue::Array(reference) => {
                let id = reference
                    .first()
                    .and_then(HostValue::as_f64)
                    .filter(|n| n.fract() == 0.0 && *n >= 0.0 && (*n as usize) < self.nodes.len())
                    .ok_or_else(|| HostError::type_error("invalid diff graph reference"))?;
                self.node(id as usize)
            }
            other => other.to_strict(),
        }
    }

    fn node(&mut self, id: usize) -> HostResult<Value> {
        if let Some(value) = &self.built[id] {
            return Ok(value.clone());
        }
        if self.building[id] {
            return Err(HostError::type_error(
                "Value contains cycles and is not strict JSON",
            ));
        }
        self.building[id] = true;
        let HostValue::Array(parts) = &self.nodes[id] else {
            return Err(HostError::type_error("invalid diff graph node"));
        };
        let value = match parts.first().and_then(HostValue::as_f64) {
            Some(0.0) => {
                let mut items = Vec::with_capacity(parts.len() - 1);
                for code in &parts[1..] {
                    items.push(self.child(code)?);
                }
                Value::Array(Arc::new(items))
            }
            Some(1.0) => {
                let mut map = Map::with_capacity((parts.len() - 1) / 2);
                for pair in parts[1..].chunks(2) {
                    let key = pair[0]
                        .as_string()
                        .ok_or_else(|| HostError::type_error("invalid diff graph key"))?
                        .clone();
                    let child = self.child(pair.get(1).unwrap_or(&HostValue::Undefined))?;
                    map.insert(key, child);
                }
                Value::Object(Arc::new(map))
            }
            _ => return Err(HostError::type_error("invalid diff graph node")),
        };
        if let Some(ptr) = container_ptr(&value) {
            self.index.insert(ptr, id);
        }
        self.building[id] = false;
        self.built[id] = Some(value.clone());
        Ok(value)
    }

    /// A payload as a node reference when it is a graph container, else inline.
    fn code(&self, value: &Value) -> HostValue {
        match container_ptr(value).and_then(|ptr| self.index.get(&ptr)) {
            Some(id) => HostValue::Array(vec![HostValue::from(*id)]),
            None if container_ptr(value).is_some() => {
                HostValue::object([("v", HostValue::from_strict(value))])
            }
            None => HostValue::from_strict(value),
        }
    }
}

fn path_host(path: &[Seg]) -> HostValue {
    HostValue::Array(
        path.iter()
            .map(|segment| match segment {
                Seg::Key(key) => HostValue::String(key.clone()),
                Seg::Index(index) => HostValue::from(*index),
            })
            .collect(),
    )
}

fn diff_op(graph: &Graph<'_>, op: &Op) -> HostValue {
    let verb = |v: &str| HostValue::string(v);
    HostValue::Array(match op {
        Op::Replace(value) => vec![verb("r"), graph.code(value)],
        Op::Set(path, value) => vec![verb("s"), path_host(path), graph.code(value)],
        Op::Splice(path, index, remove, items) => vec![
            verb("p"),
            path_host(path),
            HostValue::from(*index),
            HostValue::from(*remove),
            HostValue::Array(items.iter().map(|item| graph.code(item)).collect()),
        ],
        other => return op_to_host(other),
    })
}

pub(super) fn register(m: &mut Methods) {
    m.sync("delta.assertValidOp", |_, args| {
        Op::from_value(&op_arg(&args, 0)?).map_err(delta_error)?;
        Ok(HostValue::Undefined)
    });
    m.sync("delta.assertValidWireOp", |_, args| {
        delta::assert_valid_wire_op(&op_arg(&args, 0)?).map_err(delta_error)?;
        Ok(HostValue::Undefined)
    });
    m.sync("delta.assertSafePath", |_, args| {
        // A root splice validates exactly the path: segments only, any length.
        let path = op_arg(&args, 0)?;
        let probe = Value::Array(Arc::new(vec![
            Value::from("p"),
            path,
            Value::from(0usize),
            Value::from(0usize),
            Value::Array(Arc::new(Vec::new())),
        ]));
        Op::from_value(&probe).map_err(delta_error)?;
        Ok(HostValue::Undefined)
    });
    m.sync("delta.overlap", |_, args| {
        let size = |i: usize| -> HostResult<usize> {
            let n = args.f64(i)?;
            Ok(if n.is_nan() || n <= 0.0 {
                0
            } else if n >= usize::MAX as f64 {
                usize::MAX
            } else {
                n as usize
            })
        };
        Ok(HostValue::from(delta::overlap_units(
            args.string(0)?.units(),
            args.string(1)?.units(),
            size(2)?,
            size(3)?,
            size(4)?,
        )))
    });
    m.sync("delta.encoder", |_, _| {
        Ok(HostValue::native(Mutex::new(Encoder::new())))
    });
    m.sync("delta.encode", |_, args| {
        let encoder = args.native::<Mutex<Encoder>>(0)?;
        let ops = ops_arg(&args, 1)?;
        let wire = encoder.lock().encode(&ops).map_err(delta_error)?;
        Ok(HostValue::Array(
            wire.iter().map(HostValue::from_strict).collect(),
        ))
    });
    m.sync("delta.decoder", |_, _| {
        Ok(HostValue::native(Mutex::new(Decoder::new())))
    });
    m.sync("delta.decode", |_, args| {
        let decoder = args.native::<Mutex<Decoder>>(0)?;
        let wire = op_arg(&args, 1)?;
        let wire = wire
            .as_array()
            .ok_or_else(|| HostError::type_error("wire batch must be an array"))?;
        // Ops decoded before a failure are dropped with the batch, as upstream throws.
        let ops = decoder.lock().decode(wire).map_err(delta_error)?;
        Ok(HostValue::Array(ops.iter().map(op_to_host).collect()))
    });
    m.sync("delta.diff", |_, args| {
        let HostValue::Array(nodes) = args.get(0) else {
            return Err(HostError::type_error("diff graph must be an array"));
        };
        let mut graph = Graph {
            nodes,
            built: vec![None; nodes.len()],
            building: vec![false; nodes.len()],
            index: HashMap::new(),
        };
        let before = graph.child(args.get(1))?;
        let after = graph.child(args.get(2))?;
        let ops = delta::diff_revisions(&before, &after);
        Ok(HostValue::Array(
            ops.iter().map(|op| diff_op(&graph, op)).collect(),
        ))
    });
}
