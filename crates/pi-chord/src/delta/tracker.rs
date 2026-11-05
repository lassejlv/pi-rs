use super::{Error, Op, Path, Seg, apply_immutable, assert_safe_path, diff::diff_operations};
use crate::JsonValue;
use parking_lot::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Open,
    Prepared,
    Consumed,
    Aborted,
}
struct Lease {
    owner: Arc<AtomicU64>,
    revision: u64,
    status: Mutex<Status>,
}
impl Lease {
    fn stale(&self) -> bool {
        self.owner.load(Ordering::Acquire) != self.revision
    }
    fn abort(&self) {
        if self.stale() {
            return;
        }
        let mut status = self.status.lock();
        if matches!(*status, Status::Open | Status::Prepared) {
            *status = Status::Aborted;
        }
    }
}
fn error(message: &str) -> Error {
    Error::Type(message.into())
}

pub struct Tracker {
    value: Arc<JsonValue>,
    owner: Arc<AtomicU64>,
}
enum Edits {
    Diff,
    Recorded {
        ops: Vec<Op>,
        untracked_base: Option<JsonValue>,
        /// Structural mutators on one array since the last flush, normalized at flush.
        pieces: Option<Pieces>,
    },
}

/// A retained run of base elements or inserted values, in draft order.
enum Piece {
    Base { start: usize, len: usize },
    Insert(Vec<JsonValue>),
}
impl Piece {
    fn len(&self) -> usize {
        match self {
            Self::Base { len, .. } => *len,
            Self::Insert(items) => items.len(),
        }
    }
}

/// Upstream's piece array for one draft array: which base elements survive, in
/// which order, and what was inserted between them. Emission depends only on the
/// pieces, so intermediate splices that cancel out leave no operations.
struct Pieces {
    path: Path,
    base_len: usize,
    pieces: Vec<Piece>,
}
impl Pieces {
    fn new(path: &[Seg], base_len: usize) -> Self {
        Self {
            path: path.to_vec(),
            base_len,
            pieces: if base_len == 0 {
                Vec::new()
            } else {
                vec![Piece::Base {
                    start: 0,
                    len: base_len,
                }]
            },
        }
    }
    /// Split at a logical index and return the pieces from it onwards.
    fn split_off(pieces: &mut Vec<Piece>, index: usize) -> Vec<Piece> {
        let mut at = 0;
        for position in 0..pieces.len() {
            let len = pieces[position].len();
            if index == at {
                return pieces.split_off(position);
            }
            if index < at + len {
                let offset = index - at;
                let right = match &mut pieces[position] {
                    Piece::Base { start, len } => {
                        let right = Piece::Base {
                            start: *start + offset,
                            len: *len - offset,
                        };
                        *len = offset;
                        right
                    }
                    Piece::Insert(items) => Piece::Insert(items.split_off(offset)),
                };
                let mut tail = pieces.split_off(position + 1);
                tail.insert(0, right);
                return tail;
            }
            at += len;
        }
        Vec::new()
    }
    fn push(pieces: &mut Vec<Piece>, piece: Piece) {
        if piece.len() == 0 {
            return;
        }
        match (pieces.last_mut(), piece) {
            (Some(Piece::Insert(items)), Piece::Insert(mut more)) => items.append(&mut more),
            (
                Some(Piece::Base { start, len }),
                Piece::Base {
                    start: next,
                    len: more,
                },
            ) if *start + *len == next => *len += more,
            (_, piece) => pieces.push(piece),
        }
    }
    fn splice(&mut self, start: usize, remove: usize, items: Vec<JsonValue>) {
        let mut right = Self::split_off(&mut self.pieces, start);
        let tail = Self::split_off(&mut right, remove);
        Self::push(&mut self.pieces, Piece::Insert(items));
        for piece in tail {
            Self::push(&mut self.pieces, piece);
        }
    }
    fn permute(&mut self, order: &[usize]) {
        let mut elements = Vec::with_capacity(order.len());
        for piece in std::mem::take(&mut self.pieces) {
            match piece {
                Piece::Base { start, len } => {
                    elements.extend((start..start + len).map(|index| Piece::Base {
                        start: index,
                        len: 1,
                    }))
                }
                Piece::Insert(items) => {
                    elements.extend(items.into_iter().map(|item| Piece::Insert(vec![item])))
                }
            }
        }
        let mut elements: Vec<_> = elements.into_iter().map(Some).collect();
        for &source in order {
            let piece = elements[source].take().expect("validated permutation");
            Self::push(&mut self.pieces, piece);
        }
    }
    /// Upstream `emitArrayOperations` for a structural array: removals from the end
    /// backwards, then a permutation of the survivors if they moved, then inserts.
    fn emit(self, ops: &mut Vec<Op>) {
        let mut retained = vec![false; self.base_len];
        let mut target = Vec::new();
        for piece in &self.pieces {
            if let Piece::Base { start, len } = piece {
                for (index, kept) in retained.iter_mut().enumerate().skip(*start).take(*len) {
                    *kept = true;
                    target.push(index);
                }
            }
        }
        let mut end = self.base_len;
        while end > 0 {
            if retained[end - 1] {
                end -= 1;
                continue;
            }
            let mut start = end - 1;
            while start > 0 && !retained[start - 1] {
                start -= 1;
            }
            ops.push(Op::Splice(
                self.path.clone(),
                start,
                end - start,
                Vec::new(),
            ));
            end = start;
        }
        if target.windows(2).any(|pair| pair[0] > pair[1]) {
            let mut rank = vec![0; self.base_len];
            for (position, index) in (0..self.base_len).filter(|i| retained[*i]).enumerate() {
                rank[index] = position;
            }
            ops.push(Op::Move(
                self.path.clone(),
                target.iter().map(|index| rank[*index]).collect(),
            ));
        }
        let mut logical = 0;
        for piece in self.pieces {
            let len = piece.len();
            if let Piece::Insert(items) = piece {
                ops.push(Op::Splice(self.path.clone(), logical, 0, items));
            }
            logical += len;
        }
    }
}
pub struct Change {
    base: Option<Arc<JsonValue>>,
    state: Option<JsonValue>,
    edits: Edits,
    lease: Arc<Lease>,
    settled: bool,
}
pub struct Prepared {
    base: Arc<JsonValue>,
    value: Arc<JsonValue>,
    ops: Arc<Vec<Op>>,
    lease: Arc<Lease>,
}

pub fn track(value: JsonValue) -> Tracker {
    Tracker::new(Arc::new(value))
}
impl Tracker {
    pub fn new(value: Arc<JsonValue>) -> Self {
        Self {
            value,
            owner: Arc::new(AtomicU64::new(0)),
        }
    }
    pub fn value(&self) -> &Arc<JsonValue> {
        &self.value
    }
    pub fn revision(&self) -> u64 {
        self.owner.load(Ordering::Acquire)
    }
    fn lease(&self, status: Status) -> Arc<Lease> {
        Arc::new(Lease {
            owner: self.owner.clone(),
            revision: self.revision(),
            status: Mutex::new(status),
        })
    }
    pub fn begin_change(&self) -> Change {
        Change {
            base: Some(self.value.clone()),
            state: Some(self.value.as_ref().clone()),
            edits: Edits::Diff,
            lease: self.lease(Status::Open),
            settled: false,
        }
    }
    pub fn prepare_replace(&self, value: Arc<JsonValue>) -> Prepared {
        let same = value.as_ref() == self.value.as_ref();
        Prepared {
            base: self.value.clone(),
            ops: if same {
                vec![]
            } else {
                vec![Op::Replace(value.as_ref().clone())]
            }
            .into(),
            value: if same { self.value.clone() } else { value },
            lease: self.lease(Status::Prepared),
        }
    }
    /// The candidate is materialized before persistence. Adoption only validates
    /// the lease and swaps the authoritative root; it performs no replay or I/O.
    pub fn adopt(&mut self, prepared: &Prepared) -> Result<(), Error> {
        if !Arc::ptr_eq(&self.owner, &prepared.lease.owner) {
            return Err(error("Prepared change belongs to a different tracker"));
        }
        let mut status = prepared.lease.status.lock();
        match *status {
            Status::Consumed => return Err(error("Prepared change has already been used")),
            Status::Aborted => return Err(error("Prepared change has been aborted")),
            Status::Open => return Err(error("Prepared change is not ready")),
            Status::Prepared => {}
        }
        if prepared.lease.stale() || !Arc::ptr_eq(&self.value, &prepared.base) {
            return Err(error("Prepared change is stale"));
        }
        self.value = prepared.value.clone();
        *status = Status::Consumed;
        self.owner.fetch_add(1, Ordering::Release);
        Ok(())
    }
}

impl Change {
    /// A change that lost its revision can never be prepared. Drop its snapshot of
    /// the obsolete root as soon as it is next touched instead of waiting for abort.
    fn release_if_stale(&mut self) {
        if !self.settled && self.lease.stale() {
            self.settled = true;
            self.state = None;
            self.base = None;
            self.edits = Edits::Diff;
        }
    }
    fn check_open(&self) -> Result<(), Error> {
        if self.settled || self.lease.stale() || *self.lease.status.lock() != Status::Open {
            return Err(error("Cannot use a settled overlay"));
        }
        Ok(())
    }
    pub fn current(&self) -> Result<&JsonValue, Error> {
        self.check_open()?;
        self.state
            .as_ref()
            .ok_or_else(|| error("Cannot use a settled overlay"))
    }
    pub fn state(&mut self) -> Result<&mut JsonValue, Error> {
        self.release_if_stale();
        self.check_open()?;
        self.flush_pieces();
        if let Edits::Recorded { untracked_base, .. } = &mut self.edits
            && untracked_base.is_none()
        {
            *untracked_base = self.state.clone();
        }
        self.state
            .as_mut()
            .ok_or_else(|| error("Cannot use a settled overlay"))
    }
    fn pending_diff(&self) -> Vec<Op> {
        let current = self.state.as_ref().expect("open change owns draft");
        match &self.edits {
            Edits::Diff => {
                diff_operations(self.base.as_ref().expect("open change owns base"), current)
            }
            Edits::Recorded {
                untracked_base: Some(base),
                ..
            } => diff_operations(base, current),
            Edits::Recorded {
                untracked_base: None,
                ..
            } => Vec::new(),
        }
    }
    /// Emit pending piece-array mutations as normalized operations.
    fn flush_pieces(&mut self) {
        if let Edits::Recorded { ops, pieces, .. } = &mut self.edits
            && let Some(pieces) = pieces.take()
        {
            pieces.emit(ops);
        }
    }
    /// Start or continue piece tracking for the array at `path`, recording every
    /// earlier edit first so operation order follows mutation order.
    fn structural(&mut self, path: &[Seg], len: usize) -> &mut Pieces {
        let pending = self.pending_diff();
        if matches!(self.edits, Edits::Diff) {
            self.edits = Edits::Recorded {
                ops: Vec::new(),
                untracked_base: None,
                pieces: None,
            };
        }
        let Edits::Recorded {
            ops,
            untracked_base,
            pieces,
        } = &mut self.edits
        else {
            unreachable!("edits are recorded")
        };
        ops.extend(pending);
        *untracked_base = None;
        if pieces.as_ref().is_some_and(|pieces| pieces.path != path)
            && let Some(previous) = pieces.take()
        {
            previous.emit(ops);
        }
        pieces.get_or_insert_with(|| Pieces::new(path, len))
    }
    fn draft_array(&mut self, path: &[Seg]) -> Result<usize, Error> {
        self.release_if_stale();
        assert_safe_path(path)?;
        let mut value = self.current()?;
        for segment in path {
            value = match (value, segment) {
                (JsonValue::Object(map), Seg::Key(key)) => map.get(key),
                (JsonValue::Array(items), Seg::Index(index)) => items.get(*index),
                _ => None,
            }
            .ok_or_else(|| Error::Path(super::path_value(path)))?;
        }
        value
            .as_array()
            .map(Vec::len)
            .ok_or_else(|| Error::Path(super::path_value(path)))
    }
    fn draft_array_mut(&mut self, path: &[Seg]) -> &mut Vec<JsonValue> {
        let state = self.state.as_mut().expect("open change owns draft");
        super::resolve_mut(state, path, false)
            .ok()
            .and_then(JsonValue::as_array_mut)
            .expect("validated draft array")
    }
    /// `Array.prototype.splice` on the draft array at `path`, with JavaScript's
    /// clamping. Like upstream's draft mutators it is normalized at prepare: base
    /// removals from the end backwards, then inserts at their final indices.
    pub fn splice(
        &mut self,
        path: &[Seg],
        start: usize,
        remove: usize,
        items: Vec<JsonValue>,
    ) -> Result<Vec<JsonValue>, Error> {
        let len = self.draft_array(path)?;
        let start = start.min(len);
        let remove = remove.min(len - start);
        if remove == 0 && items.is_empty() {
            return Ok(Vec::new());
        }
        self.structural(path, len)
            .splice(start, remove, items.clone());
        Ok(self
            .draft_array_mut(path)
            .splice(start..start + remove, items)
            .collect())
    }
    /// Reorder the draft array at `path` so element `i` becomes the old element
    /// `order[i]` (the `sort`/`reverse` mutators); emitted as an `m` permutation of
    /// the surviving base elements.
    pub fn permute(&mut self, path: &[Seg], order: &[usize]) -> Result<(), Error> {
        let len = self.draft_array(path)?;
        let mut seen = vec![false; len];
        if order.len() != len
            || order
                .iter()
                .any(|index| *index >= len || std::mem::replace(&mut seen[*index], true))
        {
            return Err(error("m permutation is not a bijection"));
        }
        self.structural(path, len).permute(order);
        let values = self.draft_array_mut(path);
        let previous = values.clone();
        for (index, source) in order.iter().enumerate() {
            values[index] = previous[*source].clone();
        }
        Ok(())
    }
    /// Apply and retain an explicit operation stream, including structural edits
    /// whose final value equals the base. Failure changes neither draft nor log.
    pub fn apply_ops(&mut self, ops: &[Op]) -> Result<(), Error> {
        self.apply_ops_checked(ops, |_| Ok(()))
    }
    /// Validate the candidate before adopting it into this open change. Object
    /// document boundaries use this to reject a scalar root without replaying twice.
    pub fn apply_ops_checked(
        &mut self,
        ops: &[Op],
        validate: impl FnOnce(&JsonValue) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.release_if_stale();
        let current = self.current()?;
        if ops.is_empty() {
            return validate(current);
        }
        let candidate = apply_immutable(Some(current), ops)?;
        validate(&candidate)?;
        self.flush_pieces();
        let pending = self.pending_diff();
        let mut recorded = match std::mem::replace(&mut self.edits, Edits::Diff) {
            Edits::Diff => Vec::new(),
            Edits::Recorded { ops, .. } => ops,
        };
        recorded.extend(pending);
        recorded.extend_from_slice(ops);
        self.state = Some(candidate);
        self.edits = Edits::Recorded {
            ops: recorded,
            untracked_base: None,
            pieces: None,
        };
        Ok(())
    }
    pub fn prepare(&mut self) -> Result<Prepared, Error> {
        self.release_if_stale();
        if self.settled {
            return Err(error("Change has already been settled"));
        }
        self.check_open()?;
        self.flush_pieces();
        let pending = self.pending_diff();
        let replay = !matches!(
            &self.edits,
            Edits::Recorded {
                untracked_base: None,
                ..
            }
        );
        let mut ops = match std::mem::replace(&mut self.edits, Edits::Diff) {
            Edits::Diff => Vec::new(),
            Edits::Recorded { ops, .. } => ops,
        };
        ops.extend(pending);
        // Replacements reset persisted path dictionaries and classify a base.
        // Earlier operations are overwritten, so keep the last replacement at
        // index zero just as a flushed reference batch does.
        if let Some(index) = ops.iter().rposition(|op| matches!(op, Op::Replace(_))) {
            ops.drain(..index);
        }
        let base = self.base.as_ref().expect("open change owns base").clone();
        let replayed = if replay && !ops.is_empty() {
            Some(Arc::new(apply_immutable(Some(&base), &ops)?))
        } else {
            None
        };
        self.base = None;
        let value = self.state.take().expect("open change owns draft");
        let value = if ops.is_empty() {
            base.clone()
        } else {
            replayed.unwrap_or_else(|| Arc::new(value))
        };
        *self.lease.status.lock() = Status::Prepared;
        self.settled = true;
        Ok(Prepared {
            base,
            value,
            ops: ops.into(),
            lease: self.lease.clone(),
        })
    }
    pub fn abort(&mut self) {
        self.lease.abort();
        self.settled = true;
        self.state = None;
        self.base = None;
        self.edits = Edits::Diff;
    }
}
impl Prepared {
    pub fn base(&self) -> &Arc<JsonValue> {
        &self.base
    }
    pub fn value(&self) -> &Arc<JsonValue> {
        &self.value
    }
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }
    /// The operation batch shared by persistence, policy callbacks and publication.
    pub fn shared_ops(&self) -> Arc<Vec<Op>> {
        self.ops.clone()
    }
    pub fn base_revision(&self) -> u64 {
        self.lease.revision
    }
    pub fn abort(&self) {
        self.lease.abort();
    }
}
