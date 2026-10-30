# Local patch to boa_ast 0.22.0

This is the crates.io release `boa_ast` 0.22.0 (Boa git `337a3668`, `core/ast`) with the fixes below,
wired into the workspace with `[patch.crates-io]` in the root `Cargo.toml`. Remove the patch
once an upstream release contains an equivalent fix.

## Computed keys of class methods are not scope-analyzed

`src/scope_analyzer.rs` runs three visitors over every script/module (`BindingCollectorVisitor`,
`BindingEscapeAnalyzer`, `ScopeIndexVisitor`). Their `visit_class_element_mut` branch for
`ClassElement::MethodDefinition` went straight to the method's parameters and body and never
visited the method name. When the name is computed (`[expr]() {}`, also getters, setters, static,
async and generator methods), functions created inside `expr` were never given their scopes, so at
run time they could not resolve any outer binding: module imports, top-level `let`/`const`, or
locals of an enclosing function all threw `ReferenceError: x is not defined`.

TypeScript down-levelling emits exactly this pattern (`[(_a = function () { ... }, Symbol.asyncIterator)]()`),
which broke `@anthropic-ai/sdk` streaming (`MessageStream`) under pi's extension runtime.

Changes:

- `src/scope_analyzer.rs`: in all three `visit_class_element_mut` implementations, the
  `MethodDefinition` branch now visits a `ClassElementName::PropertyName` key with
  `visit_property_name_mut` before the function body, in the class (name) scope, which is the
  scope the key is evaluated in (ClassDefinitionEvaluation). Private names have no expression.
  The other class element kinds were checked: public fields (instance and static) already visit
  their key; private fields, private methods (`#m`) and static blocks have no computed key.
- `src/function/class.rs`: `ClassMethodDefinition::name` is `pub(crate)` so the analyzer can
  borrow it mutably.

Every patched line is marked `PATCH(pi-rs)`. Regression tests: `crates/pi-jsrt/tests/boa_patches.rs`
and `custom_provider_anthropic_streams_with_the_sdk` in
`crates/pi-coding-agent/tests/extensions_npm.rs`.

## `this` used in a nested arrow does not escape the enclosing function

`Scope::escape_this_in_enclosing_function_scope` marks the first function scope outside the
function that contains `this`. Arrow scopes are function scopes, so a `this` inside an arrow nested
in another arrow only marked the outer arrow, never the ordinary function whose `this` it is.
Upstream Boa never reads `escaped_this`, so this was latent; the boa_engine patch "Arrows kept their
enclosing function's receiver alive" (`vendor/boa_engine/PATCH.md`) relies on it to decide whether a function's `this` must be
kept in its environment.

Change: `src/scope_analyzer.rs`, `BindingCollectorVisitor::visit_function_like`: after visiting an
arrow's body, if the arrow's own function scope had `this` escaped into it, escape it again from the
arrow's body scope, so the mark propagates through any number of nested arrows. Regression:
`arrows_that_do_not_use_this_do_not_retain_the_receiver` in `crates/pi-jsrt/tests/boa_patches.rs`.
