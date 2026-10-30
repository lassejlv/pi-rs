//! `vitest.*` methods backing the vitest shim (`js/vitest.js`); installed with
//! [`crate::HostOptions::vitest`].

use pi_jsrt::VirtualModule;

use crate::errors::HostError;
use crate::host::{Dispatch, Methods};
use crate::value::HostValue;

/// The vitest shim's method group.
pub struct VitestDispatch;

impl Dispatch for VitestDispatch {
    fn register(&self, m: &mut Methods) {
        // `vitest.mock(id, keys)`: serve the shim's mock `id` for every key. A bare key
        // (`node:fs`, `fs`, a package) becomes a virtual module under that exact name; an
        // absolute file path is aliased, so every import resolving to that file gets the mock.
        m.sync("vitest.mock", |call, args| {
            let id = args.text(0)?;
            let HostValue::Array(keys) = args.get(1) else {
                return Err(HostError::type_error("mock keys must be an array"));
            };
            let source = format!(
                "globalThis[Symbol.for(\"pi-durable.vitest.mocks\")].get({})",
                serde_json::to_string(&id).unwrap_or_default()
            );
            let spec = format!("pi-durable:mock/{id}");
            let rt = call.runtime();
            rt.set_virtual_module(spec.clone(), VirtualModule::Expr(source.clone()));
            for key in keys {
                let key = key
                    .as_string()
                    .ok_or_else(|| HostError::type_error("mock keys must be strings"))?
                    .to_string_lossy();
                if key.starts_with('/') {
                    rt.set_alias(key, spec.clone());
                } else {
                    rt.set_virtual_module(key, VirtualModule::Expr(source.clone()));
                }
            }
            Ok(HostValue::Undefined)
        });
    }
}
