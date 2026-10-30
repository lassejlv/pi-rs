# fake-indexeddb for the embedded IndexedDB gate

`examples/embedded_stock.rs` resolves `fake-indexeddb` (imported by upstream
`packages/durable/test/indexeddb-storage.test.ts`) from `node_modules/` here, so the
reference checkout stays read-only. Install the exact upstream pin (integrity as in
pi-mono's `package-lock.json`, `sha512-ihhmr0KoET51jT8R4YhbUYuLLvr1qqO6INISygtN16i8loR8kAxtAUr4+EKHTSMjL+lggaAn4OC7DRqzrNgnuQ==`):

    npm ci   # or: npm install --no-save fake-indexeddb@6.2.6
