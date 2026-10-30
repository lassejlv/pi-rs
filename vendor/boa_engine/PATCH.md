# Local patches to boa_engine 0.22.0

## Self-replacing getter inline cache

`get_by_name` previously paired the slot returned by `__get__` with the object's shape after the getter ran. A getter that replaces itself with a data property changes that shape. The next cached read then treats the data value as a callable getter.

Capture the lookup shape before `__get__` and only cache the slot if the shape has not changed. This fixes Zod's memoized shape getters, including those bundled in the Claude Agent SDK.

Regression coverage: `crates/pi-jsrt/tests/boa_patches.rs::self_replacing_getter_does_not_poison_property_cache`.
