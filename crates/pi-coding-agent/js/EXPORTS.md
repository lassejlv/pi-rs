# JavaScript extension API: export checklist

Extensions run in pi-jsrt (Boa + oxc). Every runtime export of the packages an extension can import
is listed below against the TypeScript entry points (type-only exports are erased by TypeScript
stripping and are not listed). Regenerate the lists by extracting the runtime exports of the TS
`index.ts` files and comparing them with the glue modules in this directory.

Legend: `[x]` implemented, `[~]` host-only (constructing it inside an extension is not meaningful
in pi-rs; the reason is given), `[ ]` missing (none).

| Package | Runtime exports | Implemented | Host-only | Missing |
| --- | ---: | ---: | ---: | ---: |
| `@earendil-works/pi-coding-agent` | 157 | 130 | 27 | 0 |
| `@earendil-works/pi-tui` | 91 | 88 | 3 | 0 |
| `@earendil-works/pi-ai` | 77 | 77 | 0 | 0 |
| `@earendil-works/pi-ai/compat` | 136 | 136 | 0 | 0 |
| `@earendil-works/pi-ai/models` | 11 | 11 | 0 | 0 |
| `@earendil-works/pi-ai/providers/all` | 12 | 12 | 0 | 0 |
| `@earendil-works/pi-agent-core` | 9 | 9 | 0 | 0 |

## How the modules are provided

| Specifier | Module | Backing |
| --- | --- | --- |
| `typebox`, `typebox/value`, `typebox/compile` (+ `@sinclair/typebox*` aliases) | `vendor/typebox/*` | The real typebox 1.3.27, bundled by `scripts/vendor-typebox.mjs`. Schemas are byte-identical to TS (`tests/extensions_typebox.rs` compares 67 builder cases with Node). |
| `@earendil-works/pi-ai` (+ `/compat`, `/models`, `/providers/all`, `/oauth`) | `pi-ai.js`, `pi-ai/core.js`, `pi-ai/faux.js`, `pi-ai/rust.js` | JS ports of event streams, transcripts, models store, auth helpers and the faux provider; built-in API streams, providers, model catalog, images, env keys and JSON partial parsing are Rust. The compat API registry is shared with pi's Rust registry. |
| `@earendil-works/pi-agent-core` | `pi-agent-core.js` | `Agent`, `agentLoop`, `runAgentLoop`, `runToolCall`, `streamProxy` ported to JS over the pi-ai glue; default stream function is pi-ai's compat `streamSimple` (as pi's SDK installs). |
| `@earendil-works/pi-tui` | `pi-tui.js` | JS components (Container, Box, Text, SelectList, SettingsList, Image, stacks, ScrollView, MouseRegion, autocomplete, StdinBuffer, KeybindingsManager); Input/Editor/Markdown/Loader backed by Rust components; colors, terminal images, latex, program status, width/wrapping utilities backed by `pi_tui`. `Marked` is the real marked 18.0.11 (`scripts/vendor-marked.mjs`). Mouse events reach JS components (`handleMouse`, routed by Container/Box like TS). |
| `@earendil-works/pi-coding-agent` | `pi-coding-agent.js` | Tool factories over the Rust built-in tools; truncation, frontmatter, compaction (`compact`, summaries, cut points), session-tree, skills, diff, shell, CLI parsing, image and event-bus helpers backed by Rust; theme helpers over the live Rust theme; `CustomEditor` backed by the Rust CustomEditor. |

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
- [x] `AgentSessionRuntime`: JS port over Rust AgentSessions (new/switch/fork/import/dispose)
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
- [~] `RpcClient`: host-only, spawns and drives a pi child process (SDK use, not extension use)
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
- [~] `wrapRegisteredTool`: host-only, host-internal (tool wrapping is done by the Rust runner)
- [~] `wrapRegisteredTools`: host-only, host-internal

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

