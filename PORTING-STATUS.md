# Porting status

Tracks what is ported and what is outstanding. Update when finishing a module.

## Legend

- ✅ Done
- 🟡 In progress
- ⚠️ Partial
- ⏸️ Blocked
- ❌ Not started

## Summary by crate / area

| Area | Status | Notes |
|---|---:|---|
| workspace skeleton | ✅ |  |
| pi-ai | ✅ | Core types, utils, models, providers (OpenAI/Azure/Codex/Google/Mistral/Bedrock/etc), OAuth flows, fidelity audit completed |
| pi-tui | ⚠️ | Core TUI port and fullscreen layout parity checks done; Windows `ProcessTerminal` (VT input and console mode) remains unported |
| pi-telemetry | ✅ | Contracts/recorders/metrics/schemas/render + conformance suite ported |
| pi-mcp | ✅ | JSON-RPC/types/content/client/stdio + OAuth/runtime behavior aligned |
| pi-agent | ✅ | types, loop, agent, proxy, stream-fn; TS tests ported |
| pi-jsrt | ✅ | Embedded JS runtime (Boa + oxc), async/blocking handles, host bridge |
| pi-codemode | Native djs milestone | VM replaced with pinned djs; public protocol retained. pi-jsrt/extensions remain on Boa pending upstream ESM. See [scope and semantics](DJS-ENGINE.md). |
| pi-coding-agent (overall) | 🟡 | Major groups completed; skeleton/history item still marked in-progress |
| Cross-crate cleanup/parity | ✅ | JSON behavior, fetch error semantics, renderer/flicker fixes, persistence parity verified |

## pi-coding-agent groups

| Group | Scope | Status | Notes |
|---|---|---:|---|
| Group 1 | session engine (`core::{agent_session, runtime, services, ...}`) | ✅ |  |
| Group 2 | extension runtime (`core::extensions::*`, mcp servers, JS bridge) | ✅ |  |
| Group 3 | session mgr/cwd/export/messages + compaction + export_html | ✅ |  |
| Group 4 | settings/resources/skills/prompts/keybindings | ✅ |  |
| Group 5 | models/auth/runtime config/store/composer | ✅ |  |
| Group 6 | built-in tools (`core::tools::*`) | ✅ |  |
| Group 7 | interactive mode core + viewport/renderer/editor integrations | ✅ |  |
| Group 8 | interactive message/tool components | ✅ |  |
| Group 9 | selectors/dialogs (`model`, `oauth`, `session`, etc.) | ✅ |  |
| Group 10a | CLI + non-interactive modes (`pi_main`, `cli::*`) | ✅ |  |
| Group 10b | package manager + CLI (`pi install/remove/...`) | ✅ |  |
| Group 11 | built-in MCP extension (`extensions::mcp::*`) + tests | ✅ |  |
| Group 12 | built-in extensions (`codemode`, `tool_search`, `llama`, ...) | ✅ |  |
| Skeleton tracker | `pi-coding-agent` module skeleton with `todo!(...)` bodies | 🟡 | Keep until all historical TODO references are fully retired/removed |

## Audits, tests, and regressions

| Item | Status | Notes |
|---|---:|---|
| interactive-mode behavior audit | ✅ | Completed |
| non-UI test audit (`TEST-MAP.md`, 308 TS test files mapped) | ✅ | Completed |
| pi-ai / pi-tui test coverage mapping | ✅ | `pi-ai`: all 177 files resolved (with noted partial/not-portable categories) |
| golden UI rendering fidelity harness | ✅ | In `scripts/golden/` with TS reference comparison |
| fidelity audit: pi-agent / pi-mcp / pi-telemetry / pi-codemode | ✅ | Completed |
| fidelity audit: pi-ai APIs/utils | ✅ | Completed |
| fidelity audit: settings/resources/packages/models/keybindings/trust | ✅ | Completed |
| model + thinking-level persistence parity report | ✅ | Verified no divergence |
| fullscreen flicker / renderer parity report | ✅ | Fixed |
| interactive freezes/crashes reports (`/`, `/settings`, `/model`) | ✅ | Fixed |

## Optional outstanding cleanup

| Item | Status | Notes |
|---|---:|---|
| prune stale “skeleton/in-progress” rows that are now fully done | 🟡 | Helps keep this file aligned with current state |
| convert long notes into links to focused docs | 🟡 | Improves readability as status file grows |
