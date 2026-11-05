# pi-durable

Native Rust port of the TypeScript `@earendil-works/pi-durable` package: sessions, transactions,
documents, storage (memory, JSONL, SQLite, IndexedDB, Cloudflare Durable Objects), the task
scheduler, tasks, tools, code mode and compaction, behind a public `Harness`/`Conversation` API.
Built on [`pi-chord`](../pi-chord) for JSON revisions and invocation context. Served to pi
extensions, unchanged, by [`pi-durable-jsrt`](../pi-durable-jsrt). The port is in progress; the
complete acceptance scope, test counts and open items are in
[the port status](../../porting/durable/STATUS.md).

Run an offline conversation through the native generation task, scheduler,
submission controller and model SDK:

```sh
cargo run -p pi-durable --example conversation -- "Hello"
```

The example uses the SDK's scripted faux provider and memory storage. It prints
the settled submission and committed conversation messages. It needs no API key
or JavaScript runtime. Its registry installs native generation, tool and
compaction definitions; `SchedulerOptions::models` supplies the model collection.
It also installs and renders a prompt section using the public registration helpers.

Generation supports prompt preparation, pinned requests, trailing partial
writes, retries, deferred polling/cancellation, tool rounds and run settlement.
The public `Harness` and `Conversation` APIs compose these native services.
Model collections can retain hosted assistant values until a durable writer
requests strict JSON. The default provider boundary uses the existing typed
Rust SDK; complete lossless SDK 1.4 conversion remains open.

The crate root exports the registry, native tool and section registrations,
definition helpers, generation/tool types, events, settings, watches and telemetry
schema. `define_extension`, `define_tool`, `section`, `hook`, `wrap_tool` and
`wrap_section` use those same native capabilities.

The port gate runs native tests and pinned stock pi-mono tests against Rust:

```sh
bash scripts/check-durable-port.sh /path/to/pi-mono
```

Stock selection, host capabilities and unselected cases are recorded explicitly
in the status and generated test reports.
