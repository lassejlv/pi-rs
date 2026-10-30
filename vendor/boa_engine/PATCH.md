# Local patches to boa_engine 0.22.0

## Self-replacing getter inline cache

`get_by_name` previously paired the slot returned by `__get__` with the object's shape after the getter ran. A getter that replaces itself with a data property changes that shape. The next cached read then treats the data value as a callable getter.

Capture the lookup shape before `__get__` and only cache the slot if the shape has not changed. This fixes Zod's memoized shape getters, including those bundled in the Claude Agent SDK.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::self_replacing_getter_does_not_poison_property_cache`.

## Array length fast path must not skip truncation

`Array::set_length` wrote the `length` slot directly for arrays with the template shape. That is only valid when the array does not shrink: `ArraySetLength` must delete the elements past the new length. `splice` reaches this with a smaller length when a coercion grew the array, as in `values.splice({ valueOf() { values.push(4); return 1; } }, 1, 9)`, which left index 3 behind. The fast path now runs only when the new length is at least the current one.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::splice_truncates_elements_added_by_a_coercion`.

## `for (let …)` with all-local bindings allocated a dead persistent register

For a `for (let …)` loop whose bindings are all local, the compiler still called `insert_binding` for the per-iteration copy, which is only emitted for runtime scopes. That allocated a second persistent register for each binding, never written. It kept whatever temporary last used it (for example the argument of `new WeakRef(a.b)`) reachable for the rest of the frame, so a value could not be collected while an async frame was suspended after the loop. The per-iteration indices are now only collected when the loop has a runtime scope.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::for_let_loop_does_not_keep_a_dead_temporary_alive_across_await`.

## String search from a position was quadratic

`StringIndexOf` (boa_string's `JsStr::index_of`) reaches `fromIndex` with `windows(..).skip(fromIndex)`, and the windows iterator has no `nth`, so every call walks all the windows before its start. `split`, `replaceAll` and `indexOf`/`includes` with a position call it in a loop that advances through the string, which made them quadratic: splitting a 200,000-line string took seconds even in release builds. These call sites now search the substring from `fromIndex` (`string_index_of` in `builtins/string/mod.rs`); results are unchanged.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::string_search_from_a_position_is_linear`.

## Cached `array.length = n` skipped truncation

`SetPropertyByName` cached the `length` slot of an array like any data property, so the second time a site ran `array.length = 0` (a loop, or a `beforeEach` that resets a shared array) it wrote the slot directly and left every element in place: `Reflect.ownKeys` and `Object.keys` still listed them while `length` was 0, and later pushes appended past them. Setting an array's `length` is never cached now, so it always runs `ArraySetLength`.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::cached_array_length_set_still_truncates`, and `array_length_truncation_in_every_storage_mode` (dense, sparse, holes, `Reflect.set`, `defineProperty`, computed key, per-function cache, subclass).

## Typed array `set` from a dense array of numbers

`SetTypedArrayFromArrayLike` did one generic `Get` and one `IntegerIndexedElementSet` (downcast, buffer borrow, bounds check) per element. When the source is an ordinary array whose elements are densely stored as numbers (`DenseI32`/`DenseF64`, its whole length) and the target holds Numbers, the per-element steps have no observable effect beyond the stores: there are no getters, holes, proxies or `valueOf` calls. That case now converts and stores the elements in one buffer borrow (`set_from_dense_numbers` in `builtins/typed_array/builtin.rs`). Every other source takes the spec path unchanged.

## Typed array property names skipped the numeric-key round trip

`CanonicalNumericIndexString` ran `ToNumber` and `ToString` (allocating) for every string key read or written on a typed array, including `set`, `length` and other method names, on every access. `ToString` of a Number always starts with a digit, `-`, `I` or `N`, so any other key is rejected before the round trip (`canonical_numeric_index_string` in `builtins/typed_array/object.rs`).

Microbenchmark (release runner, 4,000,000 iterations, similar machine load before and after, `Math.abs` control 480 vs 530 ms): `px.set(src, i * 4)` with a shared 4-element array 2843 → 1129–1190 ms; with a fresh array literal each call 4953 → 1898–2912 ms. This made `tools-images.test.ts`'s `pixels(4000, 1000)` fixture fit its original 5 s timeout.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::typed_array_set_from_dense_numbers_matches_the_generic_path`, `::typed_array_canonical_numeric_keys_are_still_integer_indexed`.

## `JSON.parse` evaluated the text as a script

`JSON.parse` validated the text with serde, then parsed it as a JavaScript script, bytecompiled it and ran it, even without a reviver. The embedded durable transport parses every frame, so this dominated its cost (a 72 KB context view took ~80 ms). Without a reviver, the value is now built directly from the validated UTF-16 text (`DirectJson` in `builtins/json/mod.rs`): objects get `CreateDataProperty` in source order (duplicate keys keep their first position and the last value, `__proto__` is an own data property), strings keep every code unit including lone surrogates, numbers use correctly rounded decimal conversion. Validation and error messages are unchanged; the reviver path is unchanged. About 7x faster on that view.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::json_parse_direct_builder_matches_spec_semantics`, plus a differential run against Node on the same inputs.

## `JSON.stringify` of a cyclic value says "circular", as V8 and QuickJS do

Boa threw `TypeError: cyclic object value` (SpiderMonkey's wording). Node code and the upstream code-mode tests match V8's and QuickJS's wording (`/^TypeError: .*circular/`), so the message is now V8's `Converting circular structure to JSON` (without V8's appended path description). Same error type and conditions.

Regression coverage: `crates/pi-codemode/tests/sandbox.rs::rejects_invalid_text_and_image_arguments` (checks "circular"), and the upstream `packages/codemode/test/sandbox.test.ts` case "rejects invalid text() and image() arguments" through `pi-durable-jsrt`'s embedded runner.

## Arrows kept their enclosing function's receiver alive

A function with a function scope (every class constructor, and any function whose bindings are captured) stored `this` in its function environment's `FunctionSlots`, and every arrow created inside captured that environment, so `class A { constructor(f) { held.push(() => f()); } }` kept each instance reachable for as long as the arrow lived, even though nothing reads `this` there. The bytecompiler now marks code blocks whose `this` is never read through the environment (not an arrow, no inner arrow uses `this` (`escaped_this`, which needs the boa_ast nested-arrow fix in `vendor/boa_ast/PATCH.md`), no direct eval, no `super` or `new.target`, and for classes not a derived constructor) with `CodeBlockFlags::THIS_NOT_IN_ENVIRONMENT`; for those, `function_call` and `function_construct` push `FunctionSlots` with a lexical `this`. The function's own `this` still comes from the frame (`THIS_VALUE_CACHED` is set on both paths).

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::arrows_that_do_not_use_this_do_not_retain_the_receiver`, plus a differential run against Node of `this`/`super`/`new.target`/eval/async/generator/field cases.

## Stack overflow is a catchable `RangeError`, and catching unwinds the stack

Two fixes in `vm/mod.rs`:

- `check_runtime_limits` returned `RuntimeLimitError::Recursion` / `StackSize`, engine errors no script can catch. V8 and QuickJS throw `RangeError: Maximum call stack size exceeded`, which a `try` can catch. Both limits now throw that error; they still hold, because a handler that recurses again reaches them again.
- `handle_throw` popped frames until it found a handler, but kept the unwound frames' values on the value stack. A handler that caught an error thrown deep in the stack kept that space, so after catching a stack overflow, the next calls overflowed again (in code mode the prelude failed right after the script's `catch`). The stack is now truncated to the frame the error left, as a return truncates it. A throw that reaches an early-exit frame truncates the stack the same way the early-exit branch above it does.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::deep_recursion_throws_a_catchable_range_error_and_unwinds` (sync and async, repeated catches, deep throws, a 2000-deep call afterwards), `crates/pi-codemode/tests/sandbox.rs::ends_deep_recursion_with_a_range_error`, and the upstream `packages/codemode/test/sandbox.test.ts` case "turns deep recursion into a catchable RangeError" through the embedded runner.
