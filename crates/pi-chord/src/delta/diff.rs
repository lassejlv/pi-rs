//! Port of upstream `src/delta/diff.ts`.
//!
//! Upstream aligns arrays by JavaScript identity (`===`): a container survives an
//! edit when the same object reference appears on both sides. Native values share
//! containers through `Arc`, so container identity is `Arc::ptr_eq` and scalar
//! identity is value equality, exactly as `===` treats primitives.

use super::{Op, Path, RESERVED_SEGMENTS, Seg};
use crate::json::Map;
use crate::{JsonString, JsonValue as Value};
use std::collections::HashMap;
use std::sync::Arc;

const DEFAULT_OVERLAP_SCAN: usize = 65_536;
const MAX_DELTA_OPERATIONS: usize = 4_096;
const MAX_IDENTITY_CANDIDATES: usize = 200_000;
const MAX_SEMANTIC_CELLS: usize = 65_536;

/// `===` for JSON values, usable as a `Map` key (SameValueZero; NaN is unrepresentable).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Identity<'a> {
    Null,
    Bool(bool),
    Number(u64),
    String(&'a JsonString),
    Array(*const Vec<Value>),
    Object(*const Map),
}

fn identity(value: &Value) -> Identity<'_> {
    match value {
        Value::Null => Identity::Null,
        Value::Bool(b) => Identity::Bool(*b),
        Value::Number(n) => {
            let n = n.as_f64().expect("finite JSON number");
            Identity::Number(if n == 0.0 { 0 } else { n.to_bits() })
        }
        Value::String(s) => Identity::String(s),
        Value::Array(a) => Identity::Array(Arc::as_ptr(a)),
        Value::Object(o) => Identity::Object(Arc::as_ptr(o)),
    }
}

fn same(left: &Value, right: &Value) -> bool {
    identity(left) == identity(right)
}

fn is_container(value: &Value) -> bool {
    matches!(value, Value::Array(_) | Value::Object(_))
}

/// `left === right || equalJson(left, right)`; `PartialEq` checks `Arc` identity first.
fn same_value(left: &Value, right: &Value) -> bool {
    left == right
}

struct Emitter {
    operations: Vec<Op>,
    overflowed: bool,
}

impl Emitter {
    fn emit(&mut self, operation: Op) {
        if self.overflowed {
            return;
        }
        if self.operations.len() >= MAX_DELTA_OPERATIONS {
            self.overflowed = true;
            return;
        }
        self.operations.push(operation);
    }

    fn emit_set(&mut self, path: &[Seg], value: &Value) {
        if path.is_empty() {
            self.emit(Op::Replace(value.clone()));
        } else {
            self.emit(Op::Set(path.to_vec(), value.clone()));
        }
    }

    fn emit_string(&mut self, before: &JsonString, after: &JsonString, path: &[Seg]) {
        let (a, b) = (before.units(), after.units());
        if a == b {
            return;
        }
        if b.len() > a.len() && b.starts_with(a) {
            self.emit(Op::Append(
                path.to_vec(),
                JsonString::from_utf16(b[a.len()..].to_vec()),
            ));
            return;
        }
        let shared = overlap(a, b);
        if shared == 0 {
            self.emit(Op::Set(path.to_vec(), Value::String(after.clone())));
            return;
        }
        self.emit(Op::Truncate(path.to_vec(), a.len() - shared));
        if b.len() > shared {
            self.emit(Op::Append(
                path.to_vec(),
                JsonString::from_utf16(b[shared..].to_vec()),
            ));
        }
    }

    fn diff_value(&mut self, before: &Value, after: &Value, path: &mut Path) {
        if same(before, after) || self.overflowed {
            return;
        }
        match (before, after) {
            (Value::String(a), Value::String(b)) if !path.is_empty() => {
                self.emit_string(a, b, path)
            }
            (Value::Array(a), Value::Array(b)) => self.diff_array(before, after, a, b, path),
            (Value::Object(a), Value::Object(b)) => self.diff_object(a, b, after, path),
            _ => self.emit_set(path, after),
        }
    }

    fn diff_array(
        &mut self,
        before_value: &Value,
        after_value: &Value,
        before: &[Value],
        after: &[Value],
        path: &mut Path,
    ) {
        if before_value == after_value {
            return;
        }
        if before.len() == after.len()
            && before.len() > 1
            && !same_value(&before[0], &after[0])
            && !same_value(&before[before.len() - 1], &after[after.len() - 1])
            && let Some(order) = permutation(before, after)
        {
            self.emit(Op::Move(path.clone(), order));
            return;
        }
        self.region(before, after, path, 0, before.len(), 0, after.len(), 0);
    }

    fn diff_object(&mut self, before: &Map, after: &Map, after_value: &Value, path: &mut Path) {
        if before
            .keys()
            .chain(after.keys())
            .any(|key| RESERVED_SEGMENTS.iter().any(|s| key.equals_str(s)))
        {
            if before != after {
                self.emit_set(path, after_value);
            }
            return;
        }
        for (key, value) in after {
            if self.overflowed {
                return;
            }
            path.push(Seg::Key(key.clone()));
            match before.get(key) {
                Some(old) => self.diff_value(old, value, path),
                None => self.emit_set(path, value),
            }
            path.pop();
        }
        for key in before.keys() {
            if self.overflowed {
                return;
            }
            if !after.contains_key(key) {
                path.push(Seg::Key(key.clone()));
                self.emit(Op::Delete(path.clone()));
                path.pop();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_matches(
        &mut self,
        before: &[Value],
        after: &[Value],
        path: &mut Path,
        before_end: usize,
        after_end: usize,
        mut before_at: usize,
        mut after_at: usize,
        mut output_at: usize,
        matches: &[(usize, usize)],
    ) {
        for &(before_match, after_match) in matches {
            if self.overflowed {
                return;
            }
            self.region(
                before,
                after,
                path,
                before_at,
                before_match,
                after_at,
                after_match,
                output_at,
            );
            output_at += after_match - after_at;
            if !same_value(&before[before_match], &after[after_match]) {
                path.push(Seg::Index(output_at));
                self.diff_value(&before[before_match], &after[after_match], path);
                path.pop();
            }
            output_at += 1;
            before_at = before_match + 1;
            after_at = after_match + 1;
        }
        if !self.overflowed {
            self.region(
                before, after, path, before_at, before_end, after_at, after_end, output_at,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn region(
        &mut self,
        before: &[Value],
        after: &[Value],
        path: &mut Path,
        mut before_start: usize,
        mut before_end: usize,
        mut after_start: usize,
        mut after_end: usize,
        mut output_start: usize,
    ) {
        if self.overflowed {
            return;
        }
        while before_start < before_end
            && after_start < after_end
            && same_value(&before[before_start], &after[after_start])
        {
            before_start += 1;
            after_start += 1;
            output_start += 1;
        }
        while before_start < before_end
            && after_start < after_end
            && same_value(&before[before_end - 1], &after[after_end - 1])
        {
            before_end -= 1;
            after_end -= 1;
        }
        let before_count = before_end - before_start;
        let after_count = after_end - after_start;
        if before_count == 0 && after_count == 0 {
            return;
        }
        if before_count == 0 || after_count == 0 {
            self.emit(Op::Splice(
                path.clone(),
                output_start,
                before_count,
                after[after_start..after_end].to_vec(),
            ));
            return;
        }
        let process = |this: &mut Self, path: &mut Path, matches: &[(usize, usize)]| {
            this.process_matches(
                before,
                after,
                path,
                before_end,
                after_end,
                before_start,
                after_start,
                output_start,
                matches,
            )
        };

        if before_count == after_count {
            let positional: Vec<_> = (0..before_count)
                .filter(|offset| {
                    same_value(&before[before_start + offset], &after[after_start + offset])
                })
                .map(|offset| (before_start + offset, after_start + offset))
                .collect();
            if !positional.is_empty() {
                return process(self, path, &positional);
            }
        }

        if let Some(subsequence) = identity_subsequence(
            before,
            before_start,
            before_end,
            after,
            after_start,
            after_end,
        ) && !subsequence.is_empty()
        {
            return process(self, path, &subsequence);
        }

        let anchors = identity_anchors(
            before,
            before_start,
            before_end,
            after,
            after_start,
            after_end,
        );
        if !anchors.is_empty() {
            return process(self, path, &anchors);
        }

        if let Some(semantic) = lcs_matches(
            &before[before_start..before_end],
            &after[after_start..after_end],
        ) && !semantic.is_empty()
        {
            let absolute: Vec<_> = semantic
                .into_iter()
                .map(|(b, a)| (before_start + b, after_start + a))
                .collect();
            return process(self, path, &absolute);
        }

        if before_count == 1 && after_count == 1 {
            path.push(Seg::Index(output_start));
            self.diff_value(&before[before_start], &after[after_start], path);
            path.pop();
            return;
        }
        self.emit(Op::Splice(
            path.clone(),
            output_start,
            before_count,
            after[after_start..after_end].to_vec(),
        ));
    }
}

fn overlap(a: &[u16], b: &[u16]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let tail = &a[a.len().saturating_sub(DEFAULT_OVERLAP_SCAN)..];
    for h in [64.min(b.len()), 1] {
        let mut tried = 0;
        for k in 0..tail.len() {
            if k + h > tail.len() || tail[k..k + h] != b[..h] {
                continue;
            }
            tried += 1;
            if tried > 8 {
                break;
            }
            let count = tail.len() - k;
            if count <= b.len() && tail[k..] == b[..count] {
                return count;
            }
        }
        if h == 1 {
            break;
        }
    }
    0
}

fn permutation(before: &[Value], after: &[Value]) -> Option<Vec<usize>> {
    if before.len() != after.len() {
        return None;
    }
    let mut positions: HashMap<Identity<'_>, (Vec<usize>, usize)> = HashMap::new();
    for (index, value) in before.iter().enumerate() {
        positions.entry(identity(value)).or_default().0.push(index);
    }
    let mut result = Vec::with_capacity(after.len());
    for value in after {
        let (indices, used) = positions.get_mut(&identity(value))?;
        let index = *indices.get(*used)?;
        *used += 1;
        result.push(index);
    }
    Some(result)
}

/// `semanticallyAligned`: deep-equal, or the same kind of container sharing a
/// child container by identity at the same index or key.
fn semantically_aligned(left: &Value, right: &Value) -> bool {
    if same_value(left, right) {
        return true;
    }
    match (left, right) {
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .any(|(l, r)| is_container(l) && same(l, r))
        }
        (Value::Object(left), Value::Object(right)) => left.iter().any(|(key, value)| {
            is_container(value) && right.get(key).is_some_and(|other| same(value, other))
        }),
        _ => false,
    }
}

fn lcs_matches(before: &[Value], after: &[Value]) -> Option<Vec<(usize, usize)>> {
    if before.is_empty() || after.is_empty() {
        return Some(Vec::new());
    }
    if before.len().saturating_mul(after.len()) > MAX_SEMANTIC_CELLS {
        return None;
    }
    let width = after.len() + 1;
    let mut lengths = vec![0u32; (before.len() + 1) * width];
    for left in (0..before.len()).rev() {
        for right in (0..after.len()).rev() {
            let at = left * width + right;
            lengths[at] = if semantically_aligned(&before[left], &after[right]) {
                lengths[(left + 1) * width + right + 1] + 1
            } else {
                lengths[(left + 1) * width + right].max(lengths[left * width + right + 1])
            };
        }
    }
    let mut matches = Vec::new();
    let (mut left, mut right) = (0, 0);
    while left < before.len() && right < after.len() {
        if semantically_aligned(&before[left], &after[right])
            && lengths[left * width + right] == lengths[(left + 1) * width + right + 1] + 1
        {
            matches.push((left, right));
            left += 1;
            right += 1;
        } else if lengths[(left + 1) * width + right] >= lengths[left * width + right + 1] {
            left += 1;
        } else {
            right += 1;
        }
    }
    Some(matches)
}

// Index loops mirror upstream's two-cursor scan.
#[allow(clippy::needless_range_loop)]
fn identity_subsequence(
    before: &[Value],
    before_start: usize,
    before_end: usize,
    after: &[Value],
    after_start: usize,
    after_end: usize,
) -> Option<Vec<(usize, usize)>> {
    let before_count = before_end - before_start;
    let after_count = after_end - after_start;
    let mut matches = Vec::new();
    if after_count < before_count {
        let mut before_index = before_start;
        for after_index in after_start..after_end {
            while before_index < before_end && !same(&before[before_index], &after[after_index]) {
                before_index += 1;
            }
            if before_index == before_end {
                return None;
            }
            matches.push((before_index, after_index));
            before_index += 1;
        }
        return Some(matches);
    }
    if before_count < after_count {
        let mut after_index = after_start;
        for before_index in before_start..before_end {
            while after_index < after_end && !same(&before[before_index], &after[after_index]) {
                after_index += 1;
            }
            if after_index == after_end {
                return None;
            }
            matches.push((before_index, after_index));
            after_index += 1;
        }
        return Some(matches);
    }
    None
}

fn greedy_identity_anchors(
    positions: &HashMap<Identity<'_>, Vec<usize>>,
    after: &[Value],
    after_start: usize,
    after_end: usize,
) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    let mut next = 0;
    for (after_index, value) in after.iter().enumerate().take(after_end).skip(after_start) {
        let Some(candidates) = positions.get(&identity(value)) else {
            continue;
        };
        let at = candidates.partition_point(|index| *index < next);
        let Some(&before_index) = candidates.get(at) else {
            continue;
        };
        matches.push((before_index, after_index));
        next = before_index + 1;
    }
    matches
}

/// Longest strictly increasing chain of identity matches (patience LIS).
fn identity_anchors(
    before: &[Value],
    before_start: usize,
    before_end: usize,
    after: &[Value],
    after_start: usize,
    after_end: usize,
) -> Vec<(usize, usize)> {
    let mut positions: HashMap<Identity<'_>, Vec<usize>> = HashMap::new();
    for (index, value) in before
        .iter()
        .enumerate()
        .take(before_end)
        .skip(before_start)
    {
        positions.entry(identity(value)).or_default().push(index);
    }
    let mut candidate_count = 0;
    for value in &after[after_start..after_end] {
        candidate_count += positions.get(&identity(value)).map_or(0, Vec::len);
        if candidate_count > MAX_IDENTITY_CANDIDATES {
            return greedy_identity_anchors(&positions, after, after_start, after_end);
        }
    }
    if candidate_count == 0 {
        return Vec::new();
    }
    struct Candidate {
        before: usize,
        after: usize,
        previous: Option<usize>,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut tails: Vec<usize> = Vec::new();
    let mut tail_values: Vec<usize> = Vec::new();
    for (after_index, value) in after.iter().enumerate().take(after_end).skip(after_start) {
        let Some(before_positions) = positions.get(&identity(value)) else {
            continue;
        };
        for &before_index in before_positions.iter().rev() {
            let at = tail_values.partition_point(|value| *value < before_index);
            let candidate_index = candidates.len();
            candidates.push(Candidate {
                before: before_index,
                after: after_index,
                previous: at.checked_sub(1).map(|at| tails[at]),
            });
            if at == tails.len() {
                tails.push(candidate_index);
                tail_values.push(before_index);
            } else {
                tails[at] = candidate_index;
                tail_values[at] = before_index;
            }
        }
    }
    let mut matches = Vec::new();
    let mut candidate_index = tails.last().copied();
    while let Some(index) = candidate_index {
        let candidate = &candidates[index];
        matches.push((candidate.before, candidate.after));
        candidate_index = candidate.previous;
    }
    matches.reverse();
    matches
}

fn number_cost(value: &serde_json::Number) -> usize {
    pi_json::number_to_string(value.as_f64().expect("finite JSON number")).len()
}

fn json_cost(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::String(s) => s.units().len() + 2,
        Value::Number(n) => number_cost(n),
        Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        Value::Array(values) => {
            let mut cost = 2;
            for (index, value) in values.iter().enumerate() {
                cost += json_cost(value) + usize::from(index != 0);
            }
            cost
        }
        Value::Object(map) => {
            let mut cost = 2;
            for (index, (key, value)) in map.iter().enumerate() {
                cost += key.units().len() + 3 + json_cost(value) + usize::from(index != 0);
            }
            cost
        }
    }
}

fn path_cost(path: &[Seg]) -> usize {
    let mut cost = 2;
    for (index, segment) in path.iter().enumerate() {
        cost += match segment {
            Seg::Key(key) => key.units().len() + 2,
            Seg::Index(index) => index.to_string().len(),
        } + usize::from(index != 0);
    }
    cost
}

fn operation_cost(operation: &Op) -> usize {
    match operation {
        Op::Replace(value) => 6 + json_cost(value),
        Op::Set(path, value) => 7 + path_cost(path) + json_cost(value),
        Op::Delete(path) => 6 + path_cost(path),
        Op::Append(path, text) => 7 + path_cost(path) + text.units().len() + 2,
        Op::Truncate(path, count) => 7 + path_cost(path) + count.to_string().len(),
        Op::Splice(path, start, remove, items) => {
            let mut items_cost = 2;
            for (index, item) in items.iter().enumerate() {
                items_cost += json_cost(item) + usize::from(index != 0);
            }
            10 + path_cost(path) + start.to_string().len() + remove.to_string().len() + items_cost
        }
        Op::Move(path, order) => {
            let mut order_cost = 2;
            for (index, value) in order.iter().enumerate() {
                order_cost += value.to_string().len() + usize::from(index != 0);
            }
            7 + path_cost(path) + order_cost
        }
    }
}

/// The operation batch before the snapshot-size fallback; overflow still replaces.
pub(crate) fn diff_operations(before: &Value, after: &Value) -> Vec<Op> {
    let mut emitter = Emitter {
        operations: Vec::new(),
        overflowed: false,
    };
    emitter.diff_value(before, after, &mut Vec::new());
    if emitter.overflowed {
        return vec![Op::Replace(after.clone())];
    }
    emitter.operations
}

/// Compute a compact operation batch from two immutable JSON revisions.
pub fn diff_revisions(before: &Value, after: &Value) -> Vec<Op> {
    let operations = diff_operations(before, after);
    if operations.is_empty() || matches!(operations[0], Op::Replace(_)) {
        return operations;
    }
    let delta_cost = 2 + operations
        .iter()
        .map(|operation| operation_cost(operation) + 1)
        .sum::<usize>();
    if delta_cost < 65_536 {
        return operations;
    }
    if delta_cost >= json_cost(after) + 6 {
        vec![Op::Replace(after.clone())]
    } else {
        operations
    }
}
