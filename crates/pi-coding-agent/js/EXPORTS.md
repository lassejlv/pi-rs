# JavaScript extension API: export checklist

Extensions run in pi-jsrt (QuickJS + oxc). Every runtime export of the packages an extension can import
is listed below against the TypeScript entry points (type-only exports are erased by TypeScript
stripping and are not listed). Regenerate the lists by extracting the runtime exports of the TS
`index.ts` files and comparing them with the glue modules in this directory.

Legend: `[x]` implemented, `[~]` host-only (constructing it inside an extension is not meaningful
in pi-rs; the reason is given), `[ ]` missing (only the Chord APIs outside the durable subset; the reason is given).

| Package | Runtime exports | Implemented | Host-only | Missing |
| --- | ---: | ---: | ---: | ---: |
| `@earendil-works/pi-coding-agent` | 157 | 137 | 20 | 0 |
| `@earendil-works/pi-tui` | 91 | 88 | 3 | 0 |
| `@earendil-works/pi-ai` | 77 | 77 | 0 | 0 |
| `@earendil-works/pi-ai/compat` | 136 | 136 | 0 | 0 |
| `@earendil-works/pi-ai/models` | 11 | 11 | 0 | 0 |
| `@earendil-works/pi-ai/providers/all` | 12 | 12 | 0 | 0 |
| `@earendil-works/pi-agent-core` | 9 | 9 | 0 | 0 |
| `@earendil-works/pi-durable` | 43 | 43 | 0 | 0 |
| `@earendil-works/pi-durable/env` | 11 | 11 | 0 | 0 |
| `@earendil-works/pi-durable/env/node` | 1 | 1 | 0 | 0 |
| `@earendil-works/pi-durable/codemode` | 6 | 6 | 0 | 0 |
| `@earendil-works/pi-durable/tool-search` | 4 | 4 | 0 | 0 |
| `@earendil-works/pi-durable/tools` | 8 | 8 | 0 | 0 |
| `@earendil-works/pi-durable/images` | 1 | 1 | 0 | 0 |
| `@earendil-works/pi-durable/images/node` | 1 | 1 | 0 | 0 |
| `@earendil-works/pi-durable/images/cloudflare` | 1 | 1 | 0 | 0 |
| `@earendil-works/pi-durable/storage/memory` | 1 | 1 | 0 | 0 |
| `@earendil-works/pi-durable/storage/indexeddb` | 1 | 1 | 0 | 0 |
| `@earendil-works/pi-durable/storage/jsonl` | 3 | 3 | 0 | 0 |
| `@earendil-works/pi-durable/storage/jsonl/node` | 2 | 2 | 0 | 0 |
| `@earendil-works/pi-durable/storage/sqlite` | 4 | 4 | 0 | 0 |
| `@earendil-works/pi-durable/storage/sqlite/cloudflare` | 2 | 2 | 0 | 0 |
| `@earendil-works/pi-durable/storage/sqlite/node` | 3 | 3 | 0 | 0 |
| `@earendil-works/pi-durable/testing` | 12 | 12 | 0 | 0 |
| `@earendil-works/pi-durable/package.json` | 1 | 1 | 0 | 0 |
| `@earendil-works/chord` | 27 | 8 | 0 | 19 |
| `@earendil-works/chord/context` | 10 | 10 | 0 | 0 |
| `@earendil-works/chord/output` | 7 | 7 | 0 | 0 |
| `@earendil-works/chord/delta` | 16 | 16 | 0 | 0 |
| `@earendil-works/chord/node` | 5 | 0 | 0 | 5 |
| `@earendil-works/chord/telemetry-sqlite` | 1 | 0 | 0 | 1 |
| `@earendil-works/chord/sqlite` | 2 | 2 | 0 | 0 |
| `@earendil-works/chord/sqlite/node` | 2 | 2 | 0 | 0 |
| `@earendil-works/chord/sqlite/cloudflare` | 1 | 1 | 0 | 0 |
| `@earendil-works/chord/package.json` | 1 | 1 | 0 | 0 |
| `@earendil-works/pi-telemetry` | 7 | 7 | 0 | 0 |
| `@earendil-works/pi-telemetry/render` | 1 | 1 | 0 | 0 |
| `@earendil-works/pi-telemetry/testing` | 1 | 0 | 0 | 1 |

## How the modules are provided

| Specifier | Module | Backing |
| --- | --- | --- |
| `typebox`, `typebox/value`, `typebox/compile` (+ `@sinclair/typebox*` aliases) | `vendor/typebox/*` | The real typebox 1.3.27, bundled by `scripts/vendor-typebox.mjs`. Schemas are byte-identical to TS (`tests/extensions_typebox.rs` compares 67 builder cases with Node). |
| `@earendil-works/pi-ai` (+ `/compat`, `/models`, `/providers/all`, `/oauth`) | `pi-ai.js`, `pi-ai/core.js`, `pi-ai/faux.js`, `pi-ai/rust.js` | JS ports of event streams, transcripts, models store, auth helpers and the faux provider; built-in API streams, providers, model catalog, images, env keys and JSON partial parsing are Rust. The compat API registry is shared with pi's Rust registry. |
| `@earendil-works/pi-agent-core` | `pi-agent-core.js` | `Agent`, `agentLoop`, `runAgentLoop`, `runToolCall`, `streamProxy` ported to JS over the pi-ai glue; default stream function is pi-ai's compat `streamSimple` (as pi's SDK installs). |
| `@earendil-works/pi-tui` | `pi-tui.js` | JS components (Container, Box, Text, SelectList, SettingsList, Image, stacks, ScrollView, MouseRegion, autocomplete, StdinBuffer, KeybindingsManager); Input/Editor/Markdown/Loader backed by Rust components; colors, terminal images, latex, program status, width/wrapping utilities backed by `pi_tui`. `Marked` is the real marked 18.0.11 (`scripts/vendor-marked.mjs`). Mouse events reach JS components (`handleMouse`, routed by Container/Box like TS). |
| `@earendil-works/pi-coding-agent` | `pi-coding-agent.js` | Tool factories over the Rust built-in tools; truncation, frontmatter, compaction (`compact`, summaries, cut points), session-tree, skills, diff, shell, CLI parsing, image and event-bus helpers backed by Rust; theme helpers over the live Rust theme; `CustomEditor` backed by the Rust CustomEditor. |
| `@earendil-works/pi-durable[/…]`, `@earendil-works/chord[/…]` | `crates/pi-durable-jsrt/js/**` (installed by `pi_durable_jsrt::install` in the extension runtime) | Native pi-durable and pi-chord: Session/Tx/documents/storage (memory, JSONL, SQLite, IndexedDB), Harness/registry/tasks, environments, built-in tools, images (native Photon), code mode, tool search, conformance drivers, Chord context/output/delta/sqlite. JS keeps only what must stay JS: definition callbacks, tool `execute`/render/hook callbacks, the delta `track` draft proxy, error classes. `tests/extensions_durable.rs` runs a Session commit/read and a faux Harness turn from an extension. The TS reference test files run unchanged against the same modules (`porting/durable/mappings/embedded-*.json`). |
| `@earendil-works/pi-telemetry` (+ `/render`) | `crates/pi-durable-jsrt/js/pi-telemetry/*` | `InMemoryTelemetryContext` and `renderTrace` over the native pi-telemetry crate (spans, carriers, links, events, statuses, measurements, store queries; the JS clocks drive its timestamps); the no-op capabilities and schema helpers are the TS reference's JS. Served with the embedded packages so they need nothing from `node_modules`. |

## Notes on behavior

- Shared module state. In TS, extensions share pi's module instances. pi-rs mirrors the parts that
  matter: `getKeybindings()` returns pi's live bindings, `initTheme()` sets pi's theme, the compat API
  registry is pi's, `Image` uses pi's PNG transcoder when none is set from JS. `setKeybindings()`
  replaces pi-tui's global keybindings for the whole app (pi's editor, selectors and Rust-backed
  components): a JS `KeybindingsManager` is mirrored by a Rust manager (re-synced when it rebuilds),
  a keybindings proxy installs the Rust manager it stands for.
- `getNativeClipboard()` is pi-tui's native clipboard (`arboard`): present where TS loads its native
  addon (macOS, Windows, Linux with an X11 `DISPLAY`), with `getText`, `getImage` (PNG bytes),
  `getFilePaths`, and `setText` except on Linux (as in TS).
- pi-ai telemetry: the telemetry parent pi passes in stream options reaches JavaScript as an opaque
  `options.telemetry`; requests of a JavaScript `Models` collection that forward it are observed by
  pi-ai's Rust telemetry module (`pi.ai.request`/`pi.ai.poll` spans, events, metrics).
- Stacks and scroll views (`VStack`, `HStack`, `ScrollView`) take part in pi's fullscreen layout through
  `[LAYOUT_NODE]()` like TS; a JS `ScrollView` laid out by pi delegates its scroll state to a Rust
  `ScrollView` (wheel scrolling, follow-end, scrollbar).
- TypeBox loads on first use: pi's glue no longer imports it eagerly.

## Checklist

## `@earendil-works/pi-coding-agent`

- [x] `AgentSession`: the class of sessions from `createAgentSession()` (a Rust AgentSession: prompt/steer/followUp/abort, subscribe, state and tool getters, models, compaction, bindExtensions, `agent.beforeToolCall`/`afterToolCall`, ...); not constructible directly
- [x] `AgentSessionRuntime`: JS port of the TS class over Rust AgentSessions (newSession/switchSession/fork/importFromJsonl/dispose, rebind and before-invalidate hooks); `tests/extensions_context_api.rs` compares it with TS
- [~] `ArminComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [x] `AssistantMessageComponent`: pi's Rust component (constructor arguments and methods as in TS)
- [x] `BashExecutionComponent`: pi's Rust component (constructor arguments and methods as in TS)
- [x] `BorderedLoader`
- [x] `BranchSummaryMessageComponent`: pi's Rust component (constructor arguments and methods as in TS)
- [x] `CONFIG_DIR_NAME`
- [x] `CURRENT_SESSION_VERSION`
- [x] `CompactionSummaryMessageComponent`: pi's Rust component (constructor arguments and methods as in TS)
- [x] `CredentialSynchronizationError`
- [x] `CustomEditor`
- [x] `CustomMessageComponent`: pi's Rust component (constructor arguments and methods as in TS)
- [x] `DEFAULT_COMPACTION_SETTINGS`
- [x] `DEFAULT_MAX_BYTES`
- [x] `DEFAULT_MAX_LINES`
- [x] `DefaultPackageManager`: the Rust package manager (resolve, install/remove/update, settings sources, listConfiguredPackages, getInstalledPath, progress callback)
- [x] `DefaultResourceLoader`: the Rust loader; inline `extensionFactories` (JS functions and built-in factories), path options, skills/prompts/agents-files/system-prompt/extensions overrides (`themesOverride` and `eventBus` are not supported)
- [x] `DynamicBorder`
- [~] `ExtensionEditorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [~] `ExtensionInputComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [~] `ExtensionRunner`: host-only, the runner is the Rust host that loads the extension
- [~] `ExtensionSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [~] `FooterComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [~] `InteractiveMode`: host-only, the interactive mode is the host UI
- [~] `LoginDialogComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [x] `ModelRegistry`: the class of `ctx.modelRegistry` (every TS method over pi's live model runtime; `instanceof` works)
- [x] `ModelRuntime`: `ModelRuntime.create()` and `ctx.modelRegistry.runtime` (the host runtime); every public method (a JS `credentials` store option is not supported)
- [~] `ModelSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [~] `OAuthSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [x] `ProjectTrustStore`: the Rust trust store (`get`/`getEntry`/`set`/`setMany`)
- [x] `RpcClient`: JS port; without `cliPath` it spawns this pi binary, a `.js`/`.ts` `cliPath` runs under `node` as in TS, any other `cliPath` is spawned as an executable
- [x] `SessionManager`: the class of `ctx.sessionManager` and of `newSession({ setup })`'s manager; constructor and static factories (`create`, `open`, `continueRecent`, `inMemory`, `forkFrom`, `findById`, `list`, `listAll`) create Rust session managers
- [~] `SessionSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [x] `SettingsManager`: `create`, `inMemory` and `fromStorage` (a JS `SettingsStorage`) build Rust settings managers; every public method
- [~] `SettingsSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [~] `ShowImagesSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [x] `SkillInvocationMessageComponent`: pi's Rust component (constructor arguments and methods as in TS)
- [x] `Theme`
- [~] `ThemeSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [~] `ThinkingSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [x] `ToolExecutionComponent`: pi's Rust component (constructor arguments and methods as in TS)
- [~] `TreeSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [x] `UserMessageComponent`: pi's Rust component (constructor arguments and methods as in TS)
- [~] `UserMessageSelectorComponent`: host-only, interactive-mode component bound to host session state (Rust); extensions get UI through ctx.ui
- [x] `VERSION`
- [x] `VIRTUAL_MODEL_STATE_ENTRY`
- [x] `buildContextEntries`
- [x] `buildSessionContext`
- [x] `buildSessionProjection`
- [x] `calculateContextTokens`
- [x] `collectEntriesForBranchSummary`
- [x] `compact`
- [x] `convertToLlm`
- [x] `convertToPng`
- [x] `copyToClipboard`
- [x] `createAgentSession`: builds a Rust AgentSession (subagents); takes the SDK objects above, `customTools` and `modelRegistry`/`modelRuntime`
- [x] `createAgentSessionFromServices`
- [x] `createAgentSessionRuntime`
- [x] `createAgentSessionServices`: Rust ModelRuntime, SettingsManager and DefaultResourceLoader
- [x] `createBashTool`
- [x] `createBashToolDefinition`
- [x] `createCodemodeExtension`: the Rust built-in factory (as a loader `extensionFactories` entry, or called with an extension's `pi`)
- [x] `createCodingTools`
- [x] `createEditTool`
- [x] `createEditToolDefinition`
- [x] `createEventBus`
- [x] `createExtensionRuntime`: a Rust extension runtime, for `LoadExtensionsResult`s of custom ResourceLoaders
- [x] `createFindTool`
- [x] `createFindToolDefinition`
- [x] `createGrepTool`
- [x] `createGrepToolDefinition`
- [x] `createLocalBashOperations`
- [x] `createLocalPowerShellOperations`
- [x] `createLsTool`
- [x] `createLsToolDefinition`
- [x] `createMcpExtension`: the Rust built-in factory; options other than `logPath` (functions: `loadConfig`, `createTransport`, ...) are not supported
- [x] `createPowerShellTool`
- [x] `createPowerShellToolDefinition`
- [x] `createReadOnlyTools`
- [x] `createReadTool`
- [x] `createReadToolDefinition`
- [x] `createSyntheticSourceInfo`
- [x] `createToolSearchExtension`: the Rust built-in factory
- [x] `createWriteTool`
- [x] `createWriteToolDefinition`
- [x] `defineTool`
- [x] `detectSupportedImageMimeTypeFromFile`
- [x] `discoverAndLoadExtensions`: pi's Rust loader; the result's extensions can be handed back to pi (custom ResourceLoaders)
- [x] `estimateTokens`
- [x] `findCutPoint`
- [x] `findTurnStartIndex`
- [x] `formatDimensionNote`
- [x] `formatReadLineRange`
- [x] `formatSize`
- [x] `formatSkillsForPrompt`
- [x] `generateBranchSummary`
- [x] `generateDiffString`
- [x] `generateSummary`
- [x] `generateSummaryWithUsage`
- [x] `generateUnifiedPatch`
- [x] `getAgentDir`
- [x] `getDocsPath`
- [x] `getExamplesPath`
- [x] `getLanguageFromPath`
- [x] `getLastAssistantUsage`
- [x] `getLatestCompactionEntry`
- [x] `getMarkdownTheme`
- [x] `getPackageDir`
- [x] `getPowerShellConfig`
- [x] `getReadmePath`
- [x] `getSelectListTheme`
- [x] `getSettingsListTheme`
- [x] `getShellConfig`
- [x] `hasTrustRequiringProjectResources`
- [x] `highlightCode`
- [x] `initTheme`
- [x] `isBashToolResult`
- [x] `isEditToolResult`
- [x] `isFindToolResult`
- [x] `isGrepToolResult`
- [x] `isLsToolResult`
- [x] `isPowerShellToolResult`
- [x] `isReadToolResult`
- [x] `isToolCallEventType`
- [x] `isWriteToolResult`
- [x] `keyHint`
- [x] `keyText`
- [x] `loadProjectContextFiles`
- [x] `loadSkills`
- [x] `loadSkillsFromDir`
- [~] `main`: host-only, the CLI entry point is the Rust binary
- [x] `migrateSessionEntries`
- [x] `parseArgs`
- [x] `parseFrontmatter`
- [x] `parseSessionEntries`
- [x] `parseSkillBlock`
- [x] `prepareBranchEntries`
- [x] `rawKeyHint`
- [x] `readStoredCredential`
- [x] `renderDiff`
- [x] `resizeImage`
- [x] `resolveCliModel`: Rust model resolution over a `ModelRuntime`
- [x] `resolveModelScopeWithDiagnostics`: Rust model scoping over a `ModelRuntime`
- [~] `runPrintMode`: host-only, host mode
- [~] `runRpcMode`: host-only, host mode
- [x] `serializeConversation`
- [x] `sessionEntryToContextMessages`
- [x] `shouldCompact`
- [x] `stripFrontmatter`
- [x] `truncateHead`
- [x] `truncateLine`
- [x] `truncateTail`
- [x] `truncateToVisualLines`
- [x] `withFileMutationQueue`
- [x] `wrapRegisteredTool`: tool contexts from the given runner (`session.extensionRunner`)
- [x] `wrapRegisteredTools`

## `@earendil-works/pi-tui`

- [x] `Box`
- [x] `CURSOR_MARKER`
- [x] `CancellableLoader`
- [x] `CombinedAutocompleteProvider`
- [x] `Container`
- [x] `Editor`
- [x] `HStack`
- [x] `Image`
- [x] `Input`
- [x] `Key`
- [x] `KeybindingsManager`
- [x] `Loader`
- [x] `Markdown`
- [x] `Marked`
- [x] `MouseRegion`
- [~] `ProcessTerminal`: host-only, the process terminal belongs to the host; extensions write through `tui.terminal`
- [x] `ScrollView`
- [x] `SelectList`
- [x] `SettingsList`
- [x] `Spacer`
- [x] `StdinBuffer`
- [x] `TUI_KEYBINDINGS`
- [x] `Text`
- [x] `TruncatedText`
- [~] `TuiAltScreen`: host-only, the alternate-screen TUI is created by the host; extensions draw into the TUI they are given
- [~] `TuiMainScreen`: host-only, the main-screen TUI is created by the host; extensions draw into the TUI they are given
- [x] `VStack`
- [x] `allocateImageId`
- [x] `backgroundAnsi`
- [x] `calculateImageRows`
- [x] `colorToHex`
- [x] `colorToOkhsl`
- [x] `colorToOklch`
- [x] `colorToRgb`
- [x] `compositeTuiLine`
- [x] `decodeKittyPrintable`
- [x] `deleteAllKittyImages`
- [x] `deleteKittyImage`
- [x] `detectCapabilities`
- [x] `encodeITerm2`
- [x] `encodeKitty`
- [x] `foregroundAnsi`
- [x] `formatProgramStatus`
- [x] `fuzzyFilter`
- [x] `fuzzyMatch`
- [x] `getCapabilities`
- [x] `getCellDimensions`
- [x] `getGifDimensions`
- [x] `getImageDimensions`
- [x] `getJpegDimensions`
- [x] `getKeybindings`
- [x] `getNativeClipboard`
- [x] `getOsc8LinkAtColumn`
- [x] `getPngDimensions`
- [x] `getTerminalColorMode`
- [x] `getWebpDimensions`
- [x] `hyperlink`
- [x] `imageFallback`
- [x] `indexedColor`
- [x] `isAppleTerminalSession`
- [x] `isFocusable`
- [x] `isKeyRelease`
- [x] `isKeyRepeat`
- [x] `isKittyProtocolActive`
- [x] `isViewportTUI`
- [x] `matchesKey`
- [x] `mixColors`
- [x] `okhslColor`
- [x] `oklabToOkhslLightness`
- [x] `oklchColor`
- [x] `parseColor`
- [x] `parseKey`
- [x] `parseTerminalColorSchemeReport`
- [x] `renderFakeCursor`
- [x] `renderImage`
- [x] `renderLatex`
- [x] `resetCapabilitiesCache`
- [x] `rgbColor`
- [x] `setCapabilities`
- [x] `setCapabilityOverrides`
- [x] `setCellDimensions`
- [x] `setImageTranscoder`
- [x] `setKeybindings`
- [x] `setKittyProtocolActive`
- [x] `sliceByColumn`
- [x] `stripTerminalSequences`
- [x] `styleText`
- [x] `styleTextWithAnsi`
- [x] `truncateToWidth`
- [x] `visibleWidth`
- [x] `wrapTextWithAnsi`

## `@earendil-works/pi-ai`

- [x] `AI_TELEMETRY_SCHEMA`
- [x] `AssistantMessageEventStream`
- [x] `DEFAULT_MAX_AGENT_RETRY_DELAY_MS`
- [x] `EventStream`
- [x] `InMemoryCredentialStore`
- [x] `InMemoryModelsStore`
- [x] `ModelsError`
- [x] `PI_AI_VERSION`
- [x] `PI_ATTRIBUTION`
- [x] `StringEnum`
- [x] `Type`
- [x] `appendAssistantMessageDiagnostic`
- [x] `calculateCost`
- [x] `clampThinkingLevel`
- [x] `cleanupSessionResources`
- [x] `collapseSystemMessages`
- [x] `contentText`
- [x] `createAssistantMessageDiagnostic`
- [x] `createAssistantMessageEventStream`
- [x] `createFauxCore`
- [x] `createInitialSystemMessage`
- [x] `createModels`
- [x] `createProvider`
- [x] `declarationsEqual`
- [x] `defaultProviderAuthContext`
- [x] `envApiKeyAuth`
- [x] `extractDiagnosticError`
- [x] `fauxAssistantMessage`
- [x] `fauxProvider`
- [x] `fauxText`
- [x] `fauxThinking`
- [x] `fauxToolCall`
- [x] `formatThrownValue`
- [x] `formatUserAgent`
- [x] `getAttributionHeaders`
- [x] `getClientVersion`
- [x] `getCurrentSystemMessage`
- [x] `getCurrentSystemPrompt`
- [x] `getCurrentTools`
- [x] `getDeclaredTools`
- [x] `getIdentityHeaders`
- [x] `getInitialSystemMessage`
- [x] `getModelType`
- [x] `getOpenCodeSessionHeaders`
- [x] `getOverflowPatterns`
- [x] `getSupportedThinkingLevels`
- [x] `getSystemMessageText`
- [x] `getToolStateChanges`
- [x] `hasApi`
- [x] `hasNonAdditiveToolChanges`
- [x] `hasToolRedefinitions`
- [x] `isContextOverflow`
- [x] `isModelType`
- [x] `isRecoverableLength`
- [x] `isRetryableAssistantError`
- [x] `lazyApi`
- [x] `lazyOAuth`
- [x] `lazyStream`
- [x] `mergeClientIdentity`
- [x] `modelsAreEqual`
- [x] `normalizeContext`
- [x] `parseJsonWithRepair`
- [x] `parseStreamingJson`
- [x] `parseToolCallArguments`
- [x] `registerSessionResourceCleanup`
- [x] `renderSystemMessageUpdate`
- [x] `repairJson`
- [x] `resolveTranscript`
- [x] `resolveTranscriptTools`
- [x] `retryAssistantCall`
- [x] `retryDelayMs`
- [x] `toToolDeclaration`
- [x] `uuidv7`
- [x] `validateToolArguments`
- [x] `validateToolCall`
- [x] `withClientIdentity`
- [x] `withoutInitialSystemMessage`

## `@earendil-works/pi-ai/compat`

- [x] `AI_TELEMETRY_SCHEMA`
- [x] `ANTHROPIC_API_KEY_ENV`
- [x] `ANTHROPIC_AUTH_TOKEN_ENV`
- [x] `ANTHROPIC_FEDERATION_RULE_ID_ENV`
- [x] `ANTHROPIC_IDENTITY_TOKEN_FILE_ENV`
- [x] `ANTHROPIC_OAUTH_TOKEN_ENV`
- [x] `ANTHROPIC_ORGANIZATION_ID_ENV`
- [x] `ANTHROPIC_SERVICE_ACCOUNT_ID_ENV`
- [x] `ANTHROPIC_WORKSPACE_ID_ENV`
- [x] `AssistantMessageEventStream`
- [x] `DEFAULT_MAX_AGENT_RETRY_DELAY_MS`
- [x] `EventStream`
- [x] `InMemoryCredentialStore`
- [x] `InMemoryModelsStore`
- [x] `ModelsError`
- [x] `PI_AI_VERSION`
- [x] `PI_ATTRIBUTION`
- [x] `StringEnum`
- [x] `Type`
- [x] `anthropicMessagesApi`
- [x] `appendAssistantMessageDiagnostic`
- [x] `azureOpenAIResponsesApi`
- [x] `bedrockConverseStreamApi`
- [x] `calculateCost`
- [x] `clampThinkingLevel`
- [x] `cleanupSessionResources`
- [x] `collapseSystemMessages`
- [x] `complete`
- [x] `completeSimple`
- [x] `contentText`
- [x] `createAssistantMessageDiagnostic`
- [x] `createAssistantMessageEventStream`
- [x] `createFauxCore`
- [x] `createInitialSystemMessage`
- [x] `createModels`
- [x] `createProvider`
- [x] `declarationsEqual`
- [x] `defaultProviderAuthContext`
- [x] `envApiKeyAuth`
- [x] `extractDiagnosticError`
- [x] `fauxAssistantMessage`
- [x] `fauxProvider`
- [x] `fauxText`
- [x] `fauxThinking`
- [x] `fauxToolCall`
- [x] `findEnvKeys`
- [x] `formatThrownValue`
- [x] `formatUserAgent`
- [x] `generateImages`
- [x] `generateImagesOpenRouter`
- [x] `getApiProvider`
- [x] `getApiProviders`
- [x] `getAttributionHeaders`
- [x] `getClientVersion`
- [x] `getCurrentSystemMessage`
- [x] `getCurrentSystemPrompt`
- [x] `getCurrentTools`
- [x] `getDeclaredTools`
- [x] `getEnvApiKey`
- [x] `getIdentityHeaders`
- [x] `getImageModel`
- [x] `getImageModels`
- [x] `getImageProviders`
- [x] `getImagesApiProvider`
- [x] `getInitialSystemMessage`
- [x] `getModel`
- [x] `getModelType`
- [x] `getModels`
- [x] `getOpenCodeSessionHeaders`
- [x] `getOverflowPatterns`
- [x] `getProviders`
- [x] `getSupportedThinkingLevels`
- [x] `getSystemMessageText`
- [x] `getToolStateChanges`
- [x] `googleGenerativeAIApi`
- [x] `googleVertexApi`
- [x] `hasApi`
- [x] `hasNonAdditiveToolChanges`
- [x] `hasToolRedefinitions`
- [x] `isContextOverflow`
- [x] `isModelType`
- [x] `isRecoverableLength`
- [x] `isRetryableAssistantError`
- [x] `lazyApi`
- [x] `lazyOAuth`
- [x] `lazyStream`
- [x] `mergeClientIdentity`
- [x] `mistralConversationsApi`
- [x] `modelsAreEqual`
- [x] `normalizeContext`
- [x] `openAICodexResponsesApi`
- [x] `openAICompletionsApi`
- [x] `openAIResponsesApi`
- [x] `parseJsonWithRepair`
- [x] `parseStreamingJson`
- [x] `parseToolCallArguments`
- [x] `piMessagesApi`
- [x] `registerApiProvider`
- [x] `registerBuiltInApiProviders`
- [x] `registerBuiltInImagesApiProviders`
- [x] `registerFauxProvider`
- [x] `registerImagesApiProvider`
- [x] `registerSessionResourceCleanup`
- [x] `renderSystemMessageUpdate`
- [x] `repairJson`
- [x] `resetApiProviders`
- [x] `resolveTranscript`
- [x] `resolveTranscriptTools`
- [x] `retryAssistantCall`
- [x] `retryDelayMs`
- [x] `setBedrockProviderModule`
- [x] `stream`
- [x] `streamAnthropic`
- [x] `streamAzureOpenAIResponses`
- [x] `streamGoogle`
- [x] `streamGoogleVertex`
- [x] `streamMistral`
- [x] `streamOpenAICodexResponses`
- [x] `streamOpenAICompletions`
- [x] `streamOpenAIResponses`
- [x] `streamSimple`
- [x] `streamSimpleAnthropic`
- [x] `streamSimpleAzureOpenAIResponses`
- [x] `streamSimpleGoogle`
- [x] `streamSimpleGoogleVertex`
- [x] `streamSimpleMistral`
- [x] `streamSimpleOpenAICodexResponses`
- [x] `streamSimpleOpenAICompletions`
- [x] `streamSimpleOpenAIResponses`
- [x] `toToolDeclaration`
- [x] `unregisterApiProviders`
- [x] `uuidv7`
- [x] `validateToolArguments`
- [x] `validateToolCall`
- [x] `withClientIdentity`
- [x] `withoutInitialSystemMessage`

## `@earendil-works/pi-ai/models`

- [x] `AI_TELEMETRY_SCHEMA`
- [x] `ModelsError`
- [x] `calculateCost`
- [x] `clampThinkingLevel`
- [x] `createModels`
- [x] `createProvider`
- [x] `getModelType`
- [x] `getSupportedThinkingLevels`
- [x] `hasApi`
- [x] `isModelType`
- [x] `modelsAreEqual`

## `@earendil-works/pi-ai/providers/all`

- [x] `builtinModels`
- [x] `builtinProviders`
- [x] `getAllBuiltinModels`
- [x] `getBuiltinClassifierModel`
- [x] `getBuiltinClassifierModels`
- [x] `getBuiltinImageModel`
- [x] `getBuiltinImageModels`
- [x] `getBuiltinModel`
- [x] `getBuiltinModelDataGeneratedAt`
- [x] `getBuiltinModels`
- [x] `getBuiltinProviders`
- [x] `radiusProvider`

## `@earendil-works/pi-agent-core`

- [x] `Agent`
- [x] `DEFAULT_AGENT_IDENTITY`
- [x] `agentLoop`
- [x] `agentLoopContinue`
- [x] `runAgentLoop`
- [x] `runAgentLoopContinue`
- [x] `runToolCall`
- [x] `setDefaultStreamFn`
- [x] `streamProxy`

## `@earendil-works/pi-durable`

- [x] `AgentDoc`
- [x] `AssistantEntry`
- [x] `CompactionEntry`
- [x] `CompactionTask`
- [x] `ConversationBusy`
- [x] `DEFAULT_COMPACTION_POLICY`
- [x] `DEFAULT_HARNESS_IDENTITY`
- [x] `DEFAULT_PROGRESS_POLICY`
- [x] `DEFAULT_RESTART_LIMIT`
- [x] `DEFAULT_RETRY_POLICY`
- [x] `DURABLE_TELEMETRY_SCHEMA`
- [x] `GenerationTask`
- [x] `Harness`
- [x] `InboxDoc`
- [x] `LiveDoc`
- [x] `MemoryStorage`
- [x] `NestedCallDoc`
- [x] `ProviderDoc`
- [x] `ROOT_CONVERSATION_ID`
- [x] `ReadAfterWrite`
- [x] `ReadOnlySession`
- [x] `ResetEntry`
- [x] `SessionFailed`
- [x] `StorageRequestError`
- [x] `SystemEntry`
- [x] `ToolResultEntry`
- [x] `ToolTask`
- [x] `UsageDoc`
- [x] `UserEntry`
- [x] `configure`
- [x] `createRegistry`
- [x] `createSession`
- [x] `defineDoc`
- [x] `defineDocFamily`
- [x] `defineEntry`
- [x] `defineExtension`
- [x] `defineTask`
- [x] `defineTool`
- [x] `hook`
- [x] `section`
- [x] `watchEvents`
- [x] `wrapSection`
- [x] `wrapTool`

## `@earendil-works/pi-durable/env`

- [x] `ExecutionError`
- [x] `FileError`
- [x] `LineScanner`
- [x] `StreamDecoder`
- [x] `err`
- [x] `getOrThrow`
- [x] `getOrUndefined`
- [x] `ok`
- [x] `rangeDecoder`
- [x] `startsWithBom`
- [x] `toError`

## `@earendil-works/pi-durable/env/node`

- [x] `NodeExecutionEnv`

## `@earendil-works/pi-durable/codemode`

- [x] `CODEMODE_MODULE_VERSION`
- [x] `CODEMODE_TOOL_NAME`
- [x] `CodemodeStoreDoc`
- [x] `DEFAULT_CODEMODE_INLINE_BUDGET`
- [x] `createCodemodeExtension`
- [x] `createCodemodeTool`

## `@earendil-works/pi-durable/tool-search`

- [x] `DEFAULT_TOOL_SEARCH_LIMIT`
- [x] `TOOL_SEARCH_TOOL_NAME`
- [x] `createToolSearchExtension`
- [x] `createToolSearchTool`

## `@earendil-works/pi-durable/tools`

- [x] `CodingTools`
- [x] `DEFAULT_IMAGE_LIMITS`
- [x] `createBashTool`
- [x] `createCodingTools`
- [x] `createEditTool`
- [x] `createPowerShellTool`
- [x] `createReadTool`
- [x] `createWriteTool`

## `@earendil-works/pi-durable/images`

- [x] `createPhotonImages`: native Photon; the WebAssembly module argument is accepted and not needed

## `@earendil-works/pi-durable/images/node`

- [x] `createNodePhotonImages`

## `@earendil-works/pi-durable/images/cloudflare`

- [x] `createCloudflarePhotonImages`: the same native Photon processor (no Workers runtime: structural)

## `@earendil-works/pi-durable/storage/memory`

- [x] `MemoryStorage`

## `@earendil-works/pi-durable/storage/indexeddb`

- [x] `IndexedDbStorage`

## `@earendil-works/pi-durable/storage/jsonl`

- [x] `JsonlCorruptionError`
- [x] `JsonlStorage`
- [x] `JsonlStoragePoisonedError`

## `@earendil-works/pi-durable/storage/jsonl/node`

- [x] `JsonlStorage`
- [x] `openNodeJsonlStorage`

## `@earendil-works/pi-durable/storage/sqlite`

- [x] `CURRENT_SQLITE_SCHEMA_VERSION`
- [x] `SQLITE_MIGRATIONS`
- [x] `SqliteStorage`
- [x] `applySqliteMigrations`

## `@earendil-works/pi-durable/storage/sqlite/cloudflare`

- [x] `DurableObjectSqliteDatabase`
- [x] `openDurableObjectSqliteStorage`

## `@earendil-works/pi-durable/storage/sqlite/node`

- [x] `NodeSqliteDatabase`
- [x] `openNodeSqliteDatabase`
- [x] `openNodeSqliteStorage`

## `@earendil-works/pi-durable/testing`

- [x] `STORAGE_MEMORY_SCALES`
- [x] `STORAGE_READ_BENCHMARKS`
- [x] `STORAGE_WRITE_BENCHMARKS`
- [x] `TIMING_SCALE`
- [x] `createEnvConformance`
- [x] `createExpectAssertions`
- [x] `createStorageConformance`
- [x] `registerEnvConformance`
- [x] `registerStorageConformance`
- [x] `seedStorageBenchmark`
- [x] `seedStorageWriteBenchmark`
- [x] `storageBenchmarkPrimaryRecordCount`

## `@earendil-works/pi-durable/package.json`

- [x] `default`

## `@earendil-works/chord`

- [x] `ChordError`
- [x] `NativeWatch`
- [ ] `PACK_THRESHOLD`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `PROTOCOL_VERSION`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [x] `Watch`
- [ ] `acceptWebSockets`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `bind`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [x] `copyJson`
- [ ] `coverage`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `createHost`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `defineFacet`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `defineRegistry`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [x] `forbidden`
- [ ] `hostFamily`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `hostNamed`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `inflateMessage`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [x] `invalid`
- [ ] `ipcLink`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [x] `isChordError`
- [x] `isJsonValue`
- [ ] `local`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `memoryLinkPair`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `method`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `opaque`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `remote`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `replicatedState`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module
- [ ] `webSocketLink`: Chord host/contract/link/facet/replicated-state API, outside the durable subset (EMBEDDED-PLAN.md section 2); not exported by the embedded module

## `@earendil-works/chord/context`

- [x] `BACKGROUND_CONTEXT`
- [x] `TODO_CONTEXT`
- [x] `awaitWithContext`
- [x] `createContextKey`
- [x] `traceIn`
- [x] `withAbortSignal`
- [x] `withCancel`
- [x] `withContextValue`
- [x] `withTelemetry`
- [x] `withoutAbortSignal`

## `@earendil-works/chord/output`

- [x] `OutputBuffer`
- [x] `PROGRESS_BYTES_PER_SECOND`
- [x] `Progress`
- [x] `boundOutput`
- [x] `characterEnd`
- [x] `sanitizeOutput`
- [x] `utf8ByteLength`

## `@earendil-works/chord/delta`

- [x] `PathError`
- [x] `RESERVED_SEGMENTS`
- [x] `UnsafePathError`
- [x] `apply`
- [x] `applyImmutable`
- [x] `applyImmutableBatches`
- [x] `assertSafePath`
- [x] `assertValidOp`
- [x] `assertValidWireOp`
- [x] `decoder`
- [x] `diffRevisions`
- [x] `encoder`
- [x] `isBase`
- [x] `isReplace`
- [x] `overlap`
- [x] `track`

## `@earendil-works/chord/node`

- [ ] `childControl`: host manager (createHostManager, forkDriver, processLock, nodeLoader, childControl) is outside the durable subset; the subpath is not served
- [ ] `createHostManager`: host manager (createHostManager, forkDriver, processLock, nodeLoader, childControl) is outside the durable subset; the subpath is not served
- [ ] `forkDriver`: host manager (createHostManager, forkDriver, processLock, nodeLoader, childControl) is outside the durable subset; the subpath is not served
- [ ] `nodeLoader`: host manager (createHostManager, forkDriver, processLock, nodeLoader, childControl) is outside the durable subset; the subpath is not served
- [ ] `processLock`: host manager (createHostManager, forkDriver, processLock, nodeLoader, childControl) is outside the durable subset; the subpath is not served

## `@earendil-works/chord/telemetry-sqlite`

- [ ] `createSqliteTelemetry`: no SQLite telemetry store natively (pi-telemetry has memory/no-op stores); the subpath is not served

## `@earendil-works/chord/sqlite`

- [x] `applySqliteMigrations`
- [x] `validateSqliteSchemaPrefix`

## `@earendil-works/chord/sqlite/node`

- [x] `NodeSqliteDatabase`
- [x] `openNodeSqliteDatabase`

## `@earendil-works/chord/sqlite/cloudflare`

- [x] `DurableObjectSqliteDatabase`

## `@earendil-works/chord/package.json`

- [x] `default`

## `@earendil-works/pi-telemetry`

- [x] `InMemoryTelemetryContext`: native recorder
- [x] `NOOP_TELEMETRY_CONTEXT`
- [x] `NOOP_TELEMETRY_METRICS`
- [x] `NOOP_TELEMETRY_PROPAGATION`
- [x] `createTypedMetrics`
- [x] `createTypedSpanStarter`
- [x] `defineTelemetrySchema`

## `@earendil-works/pi-telemetry/render`

- [x] `renderTrace`: native `render_trace`

## `@earendil-works/pi-telemetry/testing`

- [ ] `createTelemetryAdapterConformance`: not served; no durable or chord code or test imports it (the native suite is `pi_telemetry::testing::create_telemetry_adapter_conformance`)
