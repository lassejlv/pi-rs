# pi-chord

Native Rust port of the subset of `@earendil-works/chord` that `pi-durable` needs: strict JSON
ownership and UTF-16 strings, operation tuples and path handling, immutable replay, revision
diffs, invocation context and cancellation, bounded watches, output buffering, progress pacing,
and the SQLite executor/migrations.

Chord APIs outside this subset (host/contract/link/facet/replicated-state, `chord/node`,
`chord/telemetry-sqlite`) aren't ported; that's a scope decision, not a gap. See
[the port status](../../porting/durable/STATUS.md) for the exact scope and test counts.

Diff two JSON values and get back the operations that turn one into the other:

```rust
use pi_chord::JsonValue;
use pi_chord::delta::diff_revisions;

let before: JsonValue = serde_json::from_str(r#"{"count": 1}"#)?;
let after: JsonValue = serde_json::from_str(r#"{"count": 2}"#)?;
let ops = diff_revisions(&before, &after);
```

Apply operations back, immutably:

```rust
use pi_chord::delta::apply_immutable;

let updated = apply_immutable(Some(&before), &ops)?;
assert_eq!(updated, after);
```

A background invocation context with cancellation, used across durable's Session/scheduler:

```rust
use pi_chord::context::BACKGROUND_CONTEXT;
use pi_chord::AbortController;

let controller = AbortController::new();
let ctx = BACKGROUND_CONTEXT.clone();
let signal = controller.signal();
assert!(!signal.aborted());
```

## Testing

```sh
cargo test -p pi-chord
```

`examples/reference_deltas.rs` feeds native-generated diffs to the pinned TypeScript reference
verifier for cross-checking. The full port gate (native plus pinned pi-mono tests) is
`scripts/check-durable-port.sh ../pi-mono`, and the embedded-runtime gate is
`scripts/check-durable-embedded.sh ../pi-mono`.
