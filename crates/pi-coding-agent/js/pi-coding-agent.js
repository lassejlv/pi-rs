// `@earendil-works/pi-coding-agent` for extensions running in pi-jsrt.
//
// Runtime values extensions use: tool factories backed by the Rust built-in tools, truncation
// helpers, theme helpers, UI components (DynamicBorder, BorderedLoader, CustomEditor), event
// type guards and config paths. Host-only classes (sessions, runtimes, modes) are exported as
// stubs that throw when constructed.

import {
	call,
	callAsync,
	makeContext,
	RustComponent,
	rustComponent,
	themeProxy,
	tokenFor,
	u,
	keybindingsProxy,
	markRustBase,
	rustOr,
	ModelRegistry,
	SessionManager,
} from "pi-rs:bridge";
export { ModelRegistry, SessionManager };
export {
	AgentSession,
	AgentSessionRuntime,
	createAgentSession,
	createAgentSessionFromServices,
	createAgentSessionRuntime,
	createAgentSessionServices,
	createCodemodeExtension,
	createMcpExtension,
	createExtensionRuntime,
	createToolSearchExtension,
	DefaultPackageManager,
	DefaultResourceLoader,
	discoverAndLoadExtensions,
	ProjectTrustStore,
	resolveCliModel,
	resolveModelScopeWithDiagnostics,
	ModelRuntime,
	RpcClient,
	SettingsManager,
	wrapRegisteredTool,
	wrapRegisteredTools,
} from "pi-rs:sdk";
import {
	CancellableLoader,
	Container,
	Editor,
	Loader,
	Spacer,
	Text,
	getKeybindings,
	__markdownThemeRef as markdownThemeRef,
} from "@earendil-works/pi-tui";

const host = (fn, ...args) => u(call("ca", { fn, args }));
const image = async (fn, ...args) => u(await callAsync("ca.image", { fn, args })) ?? null;
const compaction = async (options) => {
	const payload = { ...options, signal: tokenFor(options.signal) ?? null, callbacks: options.callbacks ?? null, streamFn: options.streamFn ?? null };
	for (const key of Object.keys(payload)) if (payload[key] === undefined) payload[key] = null;
	return u(await callAsync("ca.compaction", payload));
};
const leafArg = (leafId) => (leafId === undefined ? { set: false } : { set: true, id: leafId });

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const config = call("ca.config");
export const CONFIG_DIR_NAME = config.configDirName;
export const VERSION = config.version;
export const getAgentDir = () => call("ca.config").agentDir;
export const getDocsPath = () => config.docsPath;
export const getExamplesPath = () => config.examplesPath;
export const getPackageDir = () => config.packageDir;
export const getReadmePath = () => config.readmePath;

// ---------------------------------------------------------------------------
// Extension helpers
// ---------------------------------------------------------------------------

export function defineTool(tool) {
	return tool;
}

export function isToolCallEventType(toolName, event) {
	return event?.toolName === toolName;
}

export const isBashToolResult = (e) => e?.toolName === "bash";
export const isPowerShellToolResult = (e) => e?.toolName === "powershell";
export const isReadToolResult = (e) => e?.toolName === "read";
export const isEditToolResult = (e) => e?.toolName === "edit";
export const isWriteToolResult = (e) => e?.toolName === "write";
export const isGrepToolResult = (e) => e?.toolName === "grep";
export const isFindToolResult = (e) => e?.toolName === "find";
export const isLsToolResult = (e) => e?.toolName === "ls";

export function createSyntheticSourceInfo(path, options) {
	return {
		path,
		source: options.source,
		scope: options.scope ?? "temporary",
		origin: options.origin ?? "top-level",
		...(options.baseDir !== undefined ? { baseDir: options.baseDir } : {}),
	};
}

// ---------------------------------------------------------------------------
// Truncation and formatting (core/tools)
// ---------------------------------------------------------------------------

const constants = call("ca.constants");
export const DEFAULT_MAX_BYTES = constants.DEFAULT_MAX_BYTES;
export const DEFAULT_MAX_LINES = constants.DEFAULT_MAX_LINES;

export const formatSize = (bytes) => host("formatSize", bytes);
export const truncateHead = (content, options) => host("truncateHead", String(content), options ?? null);
export const truncateTail = (content, options) => host("truncateTail", String(content), options ?? null);
export const truncateLine = (line, maxChars) => host("truncateLine", String(line), maxChars ?? null);
export const formatReadLineRange = (...args) => host("formatReadLineRange", ...args);
export const parseFrontmatter = (content) => host("parseFrontmatter", String(content));
export const stripFrontmatter = (content) => host("stripFrontmatter", String(content));
export const convertToLlm = (messages) => host("convertToLlm", messages);
export const serializeConversation = (messages) => host("serializeConversation", messages);
export const estimateTokens = (message) => host("estimateTokens", message);
export const getLanguageFromPath = (path) => host("getLanguageFromPath", path);
export const highlightCode = (code, lang) => host("highlightCode", code, lang ?? null) ?? String(code).split("\n");
export const generateDiffString = (...args) => host("generateDiffString", ...args);
export const parseSkillBlock = (text) => host("parseSkillBlock", text);
export const VIRTUAL_MODEL_STATE_ENTRY = "pi.virtual-model-state";

const fileMutationQueues = new Map();
let registrationQueue = Promise.resolve();

/**
 * Serialize file mutation operations targeting the same file.
 * Operations for different files still run in parallel.
 */
export async function withFileMutationQueue(filePath, fn) {
	const registration = registrationQueue.then(async () => {
		const key = host("mutationQueueKey", String(filePath));
		const currentQueue = fileMutationQueues.get(key) ?? Promise.resolve();
		let releaseNext;
		const nextQueue = new Promise((resolve) => {
			releaseNext = resolve;
		});
		const chainedQueue = currentQueue.then(() => nextQueue);
		fileMutationQueues.set(key, chainedQueue);
		return { key, currentQueue, chainedQueue, releaseNext };
	});
	registrationQueue = registration.then(
		() => undefined,
		() => undefined,
	);
	const { key, currentQueue, chainedQueue, releaseNext } = await registration;
	await currentQueue;
	try {
		return await fn();
	} finally {
		releaseNext();
		if (fileMutationQueues.get(key) === chainedQueue) fileMutationQueues.delete(key);
	}
}

// ---------------------------------------------------------------------------
// Built-in tools (Rust implementations)
// ---------------------------------------------------------------------------

function builtinTool(kind, cwd, options, definition) {
	const d = call("builtin.create", { kind, cwd, options: options ?? null });
	const id = d.id;
	const tool = {
		name: d.name,
		label: d.label,
		description: d.description,
		parameters: d.parameters,
	};
	if (d.promptSnippet !== null && d.promptSnippet !== undefined) tool.promptSnippet = d.promptSnippet;
	if (d.promptGuidelines !== null && d.promptGuidelines !== undefined) tool.promptGuidelines = d.promptGuidelines;
	if (d.renderShell !== null && d.renderShell !== undefined) tool.renderShell = d.renderShell;
	if (d.executionMode !== null && d.executionMode !== undefined) tool.executionMode = d.executionMode;
	const execute = async (toolCallId, params, signal, onUpdate, ctx) =>
		await callAsync("builtin.execute", {
			id,
			toolCallId,
			params,
			signal: tokenFor(signal) ?? null,
			onUpdate: onUpdate ?? null,
			ctx: ctx?.__ctxId ?? null,
		});
	execute.__piRust = { tool: id, kind: "execute" };
	tool.execute = execute;
	if (definition) {
		if (d.hasPrepareArguments) {
			const prepareArguments = (args) => host("builtinPrepareArguments", id, args);
			prepareArguments.__piRust = { tool: id, kind: "prepareArguments" };
			tool.prepareArguments = prepareArguments;
		}
		if (d.hasRenderCall) {
			const renderCall = (args, theme, context) =>
				rustComponent(
					call("builtin.renderCall", { id, args, theme: theme?.__themeId ?? null, context: context?.__rustContext ?? null }),
				);
			renderCall.__piRust = { tool: id, kind: "renderCall" };
			tool.renderCall = renderCall;
		}
		if (d.hasRenderResult) {
			const renderResult = (result, options, theme, context) =>
				rustComponent(
					call("builtin.renderResult", {
						id,
						result,
						options,
						theme: theme?.__themeId ?? null,
						context: context?.__rustContext ?? null,
					}),
				);
			renderResult.__piRust = { tool: id, kind: "renderResult" };
			tool.renderResult = renderResult;
		}
	}
	return tool;
}

export const createReadToolDefinition = (cwd, options) => builtinTool("read", cwd, options, true);
export const createBashToolDefinition = (cwd, options) => builtinTool("bash", cwd, options, true);
export const createEditToolDefinition = (cwd, options) => builtinTool("edit", cwd, options, true);
export const createWriteToolDefinition = (cwd, options) => builtinTool("write", cwd, options, true);
export const createGrepToolDefinition = (cwd, options) => builtinTool("grep", cwd, options, true);
export const createFindToolDefinition = (cwd, options) => builtinTool("find", cwd, options, true);
export const createLsToolDefinition = (cwd, options) => builtinTool("ls", cwd, options, true);
export const createPowerShellToolDefinition = (cwd, options) => builtinTool("powershell", cwd, options, true);
export const createReadTool = (cwd, options) => builtinTool("read", cwd, options, false);
export const createBashTool = (cwd, options) => builtinTool("bash", cwd, options, false);
export const createEditTool = (cwd, options) => builtinTool("edit", cwd, options, false);
export const createWriteTool = (cwd, options) => builtinTool("write", cwd, options, false);
export const createGrepTool = (cwd, options) => builtinTool("grep", cwd, options, false);
export const createFindTool = (cwd, options) => builtinTool("find", cwd, options, false);
export const createLsTool = (cwd, options) => builtinTool("ls", cwd, options, false);
export const createPowerShellTool = (cwd, options) => builtinTool("powershell", cwd, options, false);
export const createCodingTools = (cwd, options) => ["read", "bash", "edit", "write"].map((k) => builtinTool(k, cwd, options?.[k], false));
export const createReadOnlyTools = (cwd, options) => ["read", "grep", "find", "ls"].map((k) => builtinTool(k, cwd, options?.[k], false));

export function createLocalBashOperations(options) {
	return {
		exec: (command, cwd, execOptions) =>
			callAsync("ca.localBashExec", {
				command,
				cwd,
				shellPath: options?.shellPath ?? null,
				options: {
					onData: execOptions?.onData
						? (chunk) => {
								const bytes = chunk && chunk.$bytes ? chunk.$bytes : chunk;
								execOptions.onData(typeof Buffer !== "undefined" ? Buffer.from(bytes) : Uint8Array.from(bytes));
							}
						: null,
					signal: tokenFor(execOptions?.signal) ?? null,
					timeout: execOptions?.timeout ?? null,
					env: execOptions?.env ?? null,
				},
			}),
	};
}

export const createLocalPowerShellOperations = () => createLocalBashOperations({});

// ---------------------------------------------------------------------------
// Theme helpers
// ---------------------------------------------------------------------------

function currentTheme() {
	return themeProxy(call("ca.theme"));
}

/** pi's Markdown theme (TS `getMarkdownTheme()`): styles follow the current theme at call time. */
export function getMarkdownTheme() {
	const md = {
		heading: (t) => currentTheme().fg("mdHeading", t),
		link: (t) => currentTheme().fg("mdLink", t),
		linkUrl: (t) => currentTheme().fg("mdLinkUrl", t),
		code: (t) => currentTheme().fg("mdCode", t),
		codeBlock: (t) => currentTheme().fg("mdCodeBlock", t),
		codeBlockBorder: (t) => currentTheme().fg("mdCodeBlockBorder", t),
		quote: (t) => currentTheme().fg("mdQuote", t),
		quoteBorder: (t) => currentTheme().fg("mdQuoteBorder", t),
		hr: (t) => currentTheme().fg("mdHr", t),
		listBullet: (t) => currentTheme().fg("mdListBullet", t),
		bold: (t) => currentTheme().bold(t),
		italic: (t) => currentTheme().italic(t),
		underline: (t) => currentTheme().underline(t),
		strikethrough: (t) => currentTheme().strikethrough(t),
		highlightCode: (code, lang) => host("markdownHighlightCode", String(code), lang ?? null),
	};
	// Markdown components render this theme with the Rust `getMarkdownTheme()` directly.
	Object.defineProperty(md, "__mdTheme", { value: "default", enumerable: false });
	return md;
}

export function getSelectListTheme() {
	const theme = currentTheme();
	return {
		selectedPrefix: (t) => theme.fg("accent", t),
		selectedText: (t) => theme.fg("accent", t),
		description: (t) => theme.fg("muted", t),
		scrollInfo: (t) => theme.fg("muted", t),
		noMatch: (t) => theme.fg("muted", t),
	};
}

export function getSettingsListTheme() {
	const theme = currentTheme();
	return {
		label: (text, selected) => (selected ? theme.fg("accent", text) : text),
		value: (text, selected) => (selected ? theme.fg("accent", text) : theme.fg("muted", text)),
		description: (text) => theme.fg("dim", text),
		cursor: theme.fg("accent", "→ "),
		hint: (text) => theme.fg("dim", text),
	};
}

export function getEditorTheme() {
	const theme = currentTheme();
	return {
		borderColor: (t) => theme.fg("borderMuted", t),
		selectList: getSelectListTheme(),
	};
}

/** Sets pi's global theme, as TS `initTheme()` does for the pi-tui module extensions share. */
export const initTheme = (themeName, enableWatcher = false) => {
	host("initTheme", themeName ?? null, !!enableWatcher);
};

export class Theme {
	/** TS `new Theme(fgColors, bgColors, mode, options)`: a Rust theme behind the usual theme proxy. */
	constructor(fgColors, bgColors, mode, options = {}) {
		return themeProxy(host("newTheme", fgColors, bgColors, mode, options));
	}
}

function formatKeyPart(part, options) {
	const displayPart = call("ca.platform") === "darwin" && part.toLowerCase() === "alt" ? "option" : part;
	return options.capitalize ? displayPart.charAt(0).toUpperCase() + displayPart.slice(1) : displayPart;
}

export function formatKeyText(key, options = {}) {
	return key
		.split("/")
		.map((k) =>
			k
				.split("+")
				.map((part) => formatKeyPart(part, options))
				.join("+"),
		)
		.join("/");
}

function formatKeys(keys, options = {}) {
	if (keys.length === 0) return "";
	return formatKeyText(keys.join("/"), options);
}

export function keyText(keybinding) {
	return formatKeys(getKeybindings().getKeys(keybinding));
}

export function keyDisplayText(keybinding) {
	return formatKeys(getKeybindings().getKeys(keybinding), { capitalize: true });
}

export function keyHint(keybinding, description) {
	const theme = currentTheme();
	return theme.fg("dim", keyText(keybinding)) + theme.fg("muted", ` ${description}`);
}

export function rawKeyHint(key, description) {
	const theme = currentTheme();
	return theme.fg("dim", formatKeyText(key)) + theme.fg("muted", ` ${description}`);
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

export class DynamicBorder {
	constructor(color) {
		this.color = color ?? ((str) => currentTheme().fg("border", str));
	}

	invalidate() {}

	render(width) {
		return [this.color("─".repeat(Math.max(1, width)))];
	}
}

export class BorderedLoader extends Container {
	constructor(tui, theme, message, options) {
		super();
		this.cancellable = options?.cancellable ?? true;
		const borderColor = (s) => theme.fg("border", s);
		this.addChild(new DynamicBorder(borderColor));
		if (this.cancellable) {
			this.loader = new CancellableLoader(
				tui,
				(s) => theme.fg("accent", s),
				(s) => theme.fg("muted", s),
				message,
			);
		} else {
			this.signalController = new AbortController();
			this.loader = new Loader(
				tui,
				(s) => theme.fg("accent", s),
				(s) => theme.fg("muted", s),
				message,
			);
		}
		this.addChild(this.loader);
		if (this.cancellable) {
			this.addChild(new Spacer(1));
			this.addChild(new Text(keyHint("tui.select.cancel", "cancel"), 1, 0));
		}
		this.addChild(new Spacer(1));
		this.addChild(new DynamicBorder(borderColor));
	}

	get signal() {
		if (this.cancellable) return this.loader.signal;
		return this.signalController?.signal ?? new AbortController().signal;
	}

	set onAbort(fn) {
		if (this.cancellable) this.loader.onAbort = fn;
	}

	handleInput(data) {
		if (this.cancellable) this.loader.handleInput(data);
	}

	dispose() {
		if (typeof this.loader.dispose === "function") this.loader.dispose();
		else if (typeof this.loader.stop === "function") this.loader.stop();
	}
}

/** Editor with app-level keybindings, backed by the Rust `CustomEditor`. */
export class CustomEditor extends Editor {
	constructor(tui, theme, keybindings, options) {
		super(tui, theme, options ?? {}, "customEditor", keybindings);
		this.keybindings = keybindings;
		this.embedWorkingStatus = options?.embedWorkingStatus ?? false;
		const id = this.__rid;
		const handlers = new Map();
		const originalSet = handlers.set.bind(handlers);
		const originalDelete = handlers.delete.bind(handlers);
		handlers.set = (action, handler) => {
			originalSet(action, handler);
			call("rc", { id, m: "setActionHandler", a: [action, handler] });
			return handlers;
		};
		handlers.delete = (action) => {
			call("rc", { id, m: "setActionHandler", a: [action, null] });
			return originalDelete(action);
		};
		this.actionHandlers = handlers;
		for (const name of ["onEscape", "onCtrlD", "onPasteImage", "onExtensionShortcut"]) {
			let value;
			Object.defineProperty(this, name, {
				get: () => value,
				set: (fn) => {
					value = fn;
					call("rc", { id, m: "set_" + name, a: [fn ?? null] });
				},
				enumerable: true,
				configurable: true,
			});
		}
	}

	onAction(action, handler) {
		this.actionHandlers.set(action, handler);
	}

	// The working indicator is supplied by pi's interactive mode, which sets it on the Rust
	// CustomEditor behind this class (`as_custom_editor`); there is nothing to store in JS.
	setWorkingStatusIndicator(_indicator) {}
}

markRustBase(CustomEditor);

function unavailableClass(name) {
	return class {
		constructor() {
			throw new Error(`${name} is not available to extensions in pi-rs`);
		}
	};
}

function unavailableFn(name) {
	return () => {
		throw new Error(`${name} is not available to extensions in pi-rs`);
	};
}

export const ExtensionRunner = unavailableClass("ExtensionRunner");
export const CredentialSynchronizationError = class extends Error {};
export const InteractiveMode = unavailableClass("InteractiveMode");
export const ArminComponent = unavailableClass("ArminComponent");
export const ExtensionEditorComponent = unavailableClass("ExtensionEditorComponent");
export const ExtensionInputComponent = unavailableClass("ExtensionInputComponent");
export const ExtensionSelectorComponent = unavailableClass("ExtensionSelectorComponent");
export const FooterComponent = unavailableClass("FooterComponent");
export const LoginDialogComponent = unavailableClass("LoginDialogComponent");
export const ModelSelectorComponent = unavailableClass("ModelSelectorComponent");
export const OAuthSelectorComponent = unavailableClass("OAuthSelectorComponent");
export const SessionSelectorComponent = unavailableClass("SessionSelectorComponent");
export const SettingsSelectorComponent = unavailableClass("SettingsSelectorComponent");
export const ShowImagesSelectorComponent = unavailableClass("ShowImagesSelectorComponent");
export const ThemeSelectorComponent = unavailableClass("ThemeSelectorComponent");
export const ThinkingSelectorComponent = unavailableClass("ThinkingSelectorComponent");
export const TreeSelectorComponent = unavailableClass("TreeSelectorComponent");
export const UserMessageSelectorComponent = unavailableClass("UserMessageSelectorComponent");

// TS: core/event-bus.ts (EventEmitter semantics: handlers run in registration order, async errors are logged).
export function createEventBus() {
	let listeners = new Map();
	return {
		emit: (channel, data) => {
			for (const handler of [...(listeners.get(channel) ?? [])]) handler(data);
		},
		on: (channel, handler) => {
			const safeHandler = async (data) => {
				try {
					await handler(data);
				} catch (err) {
					console.error(`Event handler error (${channel}):`, err);
				}
			};
			if (!listeners.has(channel)) listeners.set(channel, []);
			listeners.get(channel).push(safeHandler);
			return () => {
				const list = listeners.get(channel);
				const index = list ? list.lastIndexOf(safeHandler) : -1;
				if (index >= 0) list.splice(index, 1);
			};
		},
		clear: () => {
			listeners = new Map();
		},
	};
}
export const main = unavailableFn("main");
export const runPrintMode = unavailableFn("runPrintMode");
export const runRpcMode = unavailableFn("runRpcMode");
export const parseArgs = (args) => {
	const result = host("parseArgs", args);
	result.unknownFlags = new Map(Object.entries(result.unknownFlags ?? {}));
	return result;
};
export const readStoredCredential = (providerId, authPath) => host("readStoredCredential", providerId, authPath ?? null) ?? undefined;
export const loadProjectContextFiles = (options) => host("loadProjectContextFiles", { cwd: options?.cwd ?? process.cwd(), agentDir: options?.agentDir ?? getAgentDir() });
export const loadSkills = (options = {}) =>
	host("loadSkills", {
		cwd: options.cwd ?? process.cwd(),
		agentDir: options.agentDir ?? getAgentDir(),
		skillPaths: options.skillPaths ?? [],
		includeDefaults: options.includeDefaults ?? false,
	});
export const loadSkillsFromDir = (options) => host("loadSkillsFromDir", { dir: options.dir, source: options.source });
export const formatSkillsForPrompt = (skills, fileReadTool = "read") => host("formatSkillsForPrompt", skills, fileReadTool);
export const hasTrustRequiringProjectResources = (cwd) => host("hasTrustRequiringProjectResources", String(cwd));
export const copyToClipboard = (text) => callAsync("ca.copyToClipboard", { text: String(text) });
export const convertToPng = (base64Data, mimeType) => image("convertToPng", base64Data, mimeType);
export const resizeImage = (inputBytes, mimeType, options) => image("resizeImage", Array.from(inputBytes), mimeType, options ?? null);
export const formatDimensionNote = (result) => host("formatDimensionNote", result) ?? undefined;
export const detectSupportedImageMimeTypeFromFile = (filePath) => image("detectSupportedImageMimeTypeFromFile", String(filePath));
export const getShellConfig = (customShellPath) => host("getShellConfig", customShellPath ?? null);
export const getPowerShellConfig = () => host("getPowerShellConfig");
export const renderDiff = (diffText, _options = {}) => host("renderDiff", String(diffText));
export const truncateToVisualLines = (text, maxVisualLines, width, paddingX = 0, keep = "end") =>
	host("truncateToVisualLines", String(text), maxVisualLines, width, paddingX, keep);
export const generateUnifiedPatch = (path, oldContent, newContent, contextLines = 4) =>
	host("generateUnifiedPatch", path, oldContent, newContent, contextLines);
export const calculateContextTokens = (usage) => host("calculateContextTokens", usage);
// TS: core/compaction/branch-summarization.ts
export function collectEntriesForBranchSummary(session, oldLeafId, targetId) {
	if (!oldLeafId) return { entries: [], commonAncestorId: null };
	const oldPath = new Set(session.getBranch(oldLeafId).map((e) => e.id));
	const targetPath = session.getBranch(targetId);
	let commonAncestorId = null;
	for (let i = targetPath.length - 1; i >= 0; i--) {
		if (oldPath.has(targetPath[i].id)) {
			commonAncestorId = targetPath[i].id;
			break;
		}
	}
	const entries = [];
	let current = oldLeafId;
	while (current && current !== commonAncestorId) {
		const entry = session.getEntry(current);
		if (!entry) break;
		entries.push(entry);
		current = entry.parentId;
	}
	entries.reverse();
	return { entries, commonAncestorId };
}
export const compact = (preparation, model, apiKey, headers, customInstructions, signal, thinkingLevel, streamFn, env, retry, callbacks, sessionId) =>
	compaction({ fn: "compact", preparation, model, apiKey, headers, customInstructions, signal, thinkingLevel, streamFn, env, retry, callbacks, sessionId });
export const DEFAULT_COMPACTION_SETTINGS = { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 };
export const findCutPoint = (entries, startIndex, endIndex, keepRecentTokens) =>
	host("findCutPoint", entries, startIndex, endIndex, keepRecentTokens);
export const findTurnStartIndex = (entries, entryIndex, startIndex) => host("findTurnStartIndex", entries, entryIndex, startIndex);
export const generateBranchSummary = (entries, options) => compaction({ ...options, fn: "generateBranchSummary", entries });
export const generateSummary = (messages, model, reserveTokens, apiKey, headers, signal, customInstructions, previousSummary, thinkingLevel, streamFn, env, retry, callbacks, sessionId) =>
	compaction({ fn: "generateSummary", messages, model, reserveTokens, apiKey, headers, signal, customInstructions, previousSummary, thinkingLevel, streamFn, env, retry, callbacks, sessionId });
export const generateSummaryWithUsage = (messages, model, reserveTokens, apiKey, headers, signal, customInstructions, previousSummary, thinkingLevel, streamFn, env, retry, callbacks, sessionId) =>
	compaction({ fn: "generateSummaryWithUsage", messages, model, reserveTokens, apiKey, headers, signal, customInstructions, previousSummary, thinkingLevel, streamFn, env, retry, callbacks, sessionId });
export const getLastAssistantUsage = (entries) => host("getLastAssistantUsage", entries) ?? undefined;
export const prepareBranchEntries = (entries, tokenBudget = 0) => host("prepareBranchEntries", entries, tokenBudget);
export const shouldCompact = (contextTokens, contextWindow, settings) => host("shouldCompact", contextTokens, contextWindow, settings);
export const buildContextEntries = (entries, leafId, _byId) => host("buildContextEntries", entries, leafArg(leafId));
export const buildSessionContext = (entries, leafId, _byId) => host("buildSessionContext", entries, leafArg(leafId));
export const buildSessionProjection = (entries, leafId, _byId) => host("buildSessionProjection", entries, leafArg(leafId));
export const CURRENT_SESSION_VERSION = 3;
export const getLatestCompactionEntry = (entries) => host("getLatestCompactionEntry", entries);
export const migrateSessionEntries = (entries) => {
	const migrated = host("migrateSessionEntries", entries);
	entries.splice(0, entries.length, ...migrated);
};
export const parseSessionEntries = (content) => host("parseSessionEntries", String(content));
export const sessionEntryToContextMessages = (entry) => host("sessionEntryToContextMessages", entry);

export { makeContext as __makeContext, keybindingsProxy as __keybindingsProxy, RustComponent as __RustComponent };

// ---------------------------------------------------------------------------
// Message components (pi's Rust interactive components)
// ---------------------------------------------------------------------------

const tuiArg = (tui) => (tui && typeof tui.__tuiId === "number" ? { $tui: tui.__tuiId } : null);
const nul = (v) => (v === undefined ? null : v);

function rustMessageComponent(kind, args) {
	return call("rc.new", { kind, a: args.map(nul) });
}

/** A Rust component with methods (`rc` calls). */
class RustMessageComponent extends RustComponent {
	_call(m, ...a) {
		return u(call("rc", { id: this.__rid, m, a: a.map(nul) }));
	}
	setOutputPad(padding) {
		this._call("setOutputPad", padding);
	}
}

export class AssistantMessageComponent extends RustMessageComponent {
	constructor(message, hideThinkingBlock = false, markdownTheme, hiddenThinkingLabel = "Thinking...", outputPad = 1, markdownTransformers = []) {
		super(
			rustMessageComponent("assistantMessage", [
				message,
				hideThinkingBlock,
				markdownThemeRef(markdownTheme),
				hiddenThinkingLabel,
				outputPad,
				[...(markdownTransformers ?? [])],
			]),
		);
	}
	setHideThinkingBlock(hide) {
		this._call("setHideThinkingBlock", !!hide);
	}
	setHiddenThinkingLabel(label) {
		this._call("setHiddenThinkingLabel", String(label));
	}
	updateContent(message, isStreaming) {
		this._call("updateContent", message, isStreaming);
	}
}

export class UserMessageComponent extends RustMessageComponent {
	constructor(text, markdownTheme, outputPad = 1, markdownTransformers = []) {
		super(rustMessageComponent("userMessage", [String(text ?? ""), markdownThemeRef(markdownTheme), outputPad, [...(markdownTransformers ?? [])]]));
	}
}

export class ToolExecutionComponent extends RustMessageComponent {
	constructor(toolName, toolCallId, args, options = {}, toolDefinition, ui, cwd) {
		const definition = toolDefinition
			? {
					renderShell: toolDefinition.renderShell,
					renderCall: rustOr(toolDefinition.renderCall),
					renderResult: rustOr(toolDefinition.renderResult),
				}
			: null;
		super(rustMessageComponent("toolExecution", [toolName, toolCallId, args ?? {}, options ?? {}, definition, tuiArg(ui), cwd]));
	}
	updateArgs(args) {
		this._call("updateArgs", args);
	}
	markExecutionStarted() {
		this._call("markExecutionStarted");
	}
	setArgsComplete() {
		this._call("setArgsComplete");
	}
	updateResult(result, isPartial = false) {
		this._call("updateResult", result, isPartial);
	}
	setExpanded(expanded) {
		this._call("setExpanded", !!expanded);
	}
	setShowImages(show) {
		this._call("setShowImages", !!show);
	}
	setImageWidthCells(width) {
		this._call("setImageWidthCells", width);
	}
}

class ExpandableMessageComponent extends RustMessageComponent {
	setExpanded(expanded) {
		this._call("setExpanded", !!expanded);
	}
}

export class CompactionSummaryMessageComponent extends ExpandableMessageComponent {
	constructor(message, markdownTheme, outputPad = 1) {
		super(rustMessageComponent("compactionSummary", [message, markdownThemeRef(markdownTheme), outputPad]));
	}
}

export class BranchSummaryMessageComponent extends ExpandableMessageComponent {
	constructor(message, markdownTheme, outputPad = 1) {
		super(rustMessageComponent("branchSummary", [message, markdownThemeRef(markdownTheme), outputPad]));
	}
}

export class SkillInvocationMessageComponent extends ExpandableMessageComponent {
	constructor(skillBlock, markdownTheme, outputPad = 1) {
		super(rustMessageComponent("skillInvocation", [skillBlock, markdownThemeRef(markdownTheme), outputPad]));
	}
}

export class CustomMessageComponent extends ExpandableMessageComponent {
	constructor(message, customRenderer, markdownTheme, outputPad = 1) {
		super(rustMessageComponent("customMessage", [message, customRenderer ?? null, markdownThemeRef(markdownTheme), outputPad]));
	}
}

export class BashExecutionComponent extends ExpandableMessageComponent {
	constructor(command, ui, excludeFromContext = false, outputPad = 1) {
		super(rustMessageComponent("bashExecution", [String(command), tuiArg(ui), !!excludeFromContext, outputPad]));
	}
	appendOutput(chunk) {
		this._call("appendOutput", String(chunk));
	}
	setComplete(exitCode, cancelled, truncationResult, fullOutputPath) {
		this._call("setComplete", exitCode, !!cancelled, truncationResult, fullOutputPath);
	}
	getOutput() {
		return this._call("getOutput") ?? "";
	}
	getCommand() {
		return this._call("getCommand") ?? "";
	}
}

for (const cls of [
	AssistantMessageComponent,
	UserMessageComponent,
	ToolExecutionComponent,
	CompactionSummaryMessageComponent,
	BranchSummaryMessageComponent,
	SkillInvocationMessageComponent,
	CustomMessageComponent,
	BashExecutionComponent,
]) {
	markRustBase(cls);
}
