// SDK classes of `@earendil-works/pi-coding-agent` for extensions running in pi-jsrt (virtual
// module `pi-rs:sdk`), backed by pi's Rust implementations: SettingsManager.

import {
	call,
	callAsync,
	describeNativeProvider,
	describeProviderConfig,
	describeTool,
	makeContext,
	ModelRegistry,
	rustComponent,
	SessionManager,
	streamFromHost,
	tokenFor,
	u,
} from "pi-rs:bridge";
import { ref } from "jsrt:host";

const SDK_OBJECT = Symbol("pi-rs.sdkObject");

function finalizer(op) {
	return typeof FinalizationRegistry === "function"
		? new FinalizationRegistry((id) => {
				try {
					call(op, { m: "release", id });
				} catch {}
			})
		: undefined;
}

// ---------------------------------------------------------------------------
// SettingsManager
// ---------------------------------------------------------------------------

const settingsFinalizer = finalizer("settings.static");
const hostSettings = new Map();

/** TS `SettingsManager` over a Rust settings manager (created with the static factories). */
export class SettingsManager {
	#id;

	constructor(token) {
		if (!token || typeof token !== "object" || !(SDK_OBJECT in token)) {
			throw new TypeError("SettingsManager is created with SettingsManager.create(), inMemory() or fromStorage()");
		}
		this.#id = token[SDK_OBJECT];
		if (!token.host) settingsFinalizer?.register(this, this.#id);
	}

	/** @internal The Rust settings manager id (SDK options). */
	get __piSettingsId() {
		return this.#id;
	}

	static #wrap(id) {
		return new SettingsManager({ [SDK_OBJECT]: id });
	}

	/** @internal A host settings manager (`session.settingsManager`), one object per manager. */
	static __host(id) {
		let manager = hostSettings.get(id);
		if (!manager) {
			manager = new SettingsManager({ [SDK_OBJECT]: id, host: true });
			hostSettings.set(id, manager);
		}
		return manager;
	}

	static create(cwd, agentDir, options) {
		return SettingsManager.#wrap(
			call("settings.static", { m: "create", cwd: cwd ?? process.cwd(), agentDir: agentDir ?? null, options: options ?? null }),
		);
	}

	static fromStorage(storage, options) {
		return SettingsManager.#wrap(call("settings.static", { m: "fromStorage", storage: ref(storage), options: options ?? null }));
	}

	static inMemory(settings, options) {
		return SettingsManager.#wrap(call("settings.static", { m: "inMemory", settings: settings ?? {}, options: options ?? null }));
	}

	#call(m, ...a) {
		return call("settings", { id: this.#id, m, a: a.map((v) => (v === undefined ? null : v)) });
	}

	async reload() {
		await callAsync("settings.async", { id: this.#id, m: "reload", a: [] });
	}

	async flush() {
		await callAsync("settings.async", { id: this.#id, m: "flush", a: [] });
	}

	drainErrors() {
		return (this.#call("drainErrors") ?? []).map((e) => ({ ...e, error: new Error(e.error?.message ?? "") }));
	}

	getSettings(...args) {
		return u(this.#call("getSettings", ...args));
	}
	getGlobalSettings(...args) {
		return u(this.#call("getGlobalSettings", ...args));
	}
	getProjectSettings(...args) {
		return u(this.#call("getProjectSettings", ...args));
	}
	isProjectTrusted(...args) {
		return u(this.#call("isProjectTrusted", ...args));
	}
	setProjectTrusted(...args) {
		this.#call("setProjectTrusted", ...args);
	}
	applyOverrides(...args) {
		this.#call("applyOverrides", ...args);
	}
	getLastChangelogVersion(...args) {
		return u(this.#call("getLastChangelogVersion", ...args));
	}
	setLastChangelogVersion(...args) {
		this.#call("setLastChangelogVersion", ...args);
	}
	getSessionDir(...args) {
		return u(this.#call("getSessionDir", ...args));
	}
	getDefaultProvider(...args) {
		return u(this.#call("getDefaultProvider", ...args));
	}
	getDefaultModel(...args) {
		return u(this.#call("getDefaultModel", ...args));
	}
	setDefaultProvider(...args) {
		this.#call("setDefaultProvider", ...args);
	}
	setDefaultModel(...args) {
		this.#call("setDefaultModel", ...args);
	}
	setDefaultModelAndProvider(...args) {
		this.#call("setDefaultModelAndProvider", ...args);
	}
	getSteeringMode(...args) {
		return u(this.#call("getSteeringMode", ...args));
	}
	setSteeringMode(...args) {
		this.#call("setSteeringMode", ...args);
	}
	getFollowUpMode(...args) {
		return u(this.#call("getFollowUpMode", ...args));
	}
	setFollowUpMode(...args) {
		this.#call("setFollowUpMode", ...args);
	}
	getThemeSetting(...args) {
		return u(this.#call("getThemeSetting", ...args));
	}
	getTheme(...args) {
		return u(this.#call("getTheme", ...args));
	}
	setTheme(...args) {
		this.#call("setTheme", ...args);
	}
	getDefaultThinkingLevel(...args) {
		return u(this.#call("getDefaultThinkingLevel", ...args));
	}
	setDefaultThinkingLevel(...args) {
		this.#call("setDefaultThinkingLevel", ...args);
	}
	getModelThinkingLevel(...args) {
		return u(this.#call("getModelThinkingLevel", ...args));
	}
	getAllModelThinkingLevels(...args) {
		return u(this.#call("getAllModelThinkingLevels", ...args));
	}
	setModelThinkingLevel(...args) {
		this.#call("setModelThinkingLevel", ...args);
	}
	removeModelThinkingLevel(...args) {
		this.#call("removeModelThinkingLevel", ...args);
	}
	getTransport(...args) {
		return u(this.#call("getTransport", ...args));
	}
	setTransport(...args) {
		this.#call("setTransport", ...args);
	}
	getSessionShareDestination(...args) {
		return u(this.#call("getSessionShareDestination", ...args));
	}
	setSessionShareDestination(...args) {
		this.#call("setSessionShareDestination", ...args);
	}
	getCompactionEnabled(...args) {
		return u(this.#call("getCompactionEnabled", ...args));
	}
	setCompactionEnabled(...args) {
		this.#call("setCompactionEnabled", ...args);
	}
	getCompactionReserveTokens(...args) {
		return u(this.#call("getCompactionReserveTokens", ...args));
	}
	getCompactionKeepRecentTokens(...args) {
		return u(this.#call("getCompactionKeepRecentTokens", ...args));
	}
	getCompactionSettings(...args) {
		return u(this.#call("getCompactionSettings", ...args));
	}
	getBranchSummarySettings(...args) {
		return u(this.#call("getBranchSummarySettings", ...args));
	}
	getBranchSummarySkipPrompt(...args) {
		return u(this.#call("getBranchSummarySkipPrompt", ...args));
	}
	getRetryEnabled(...args) {
		return u(this.#call("getRetryEnabled", ...args));
	}
	setRetryEnabled(...args) {
		this.#call("setRetryEnabled", ...args);
	}
	getRetrySettings(...args) {
		return u(this.#call("getRetrySettings", ...args));
	}
	getHttpIdleTimeoutMs(...args) {
		return u(this.#call("getHttpIdleTimeoutMs", ...args));
	}
	setHttpIdleTimeoutMs(...args) {
		this.#call("setHttpIdleTimeoutMs", ...args);
	}
	getCacheWarmingMode(...args) {
		return u(this.#call("getCacheWarmingMode", ...args));
	}
	setCacheWarmingMode(...args) {
		this.#call("setCacheWarmingMode", ...args);
	}
	getProviderRetrySettings(...args) {
		return u(this.#call("getProviderRetrySettings", ...args));
	}
	getWebSocketConnectTimeoutMs(...args) {
		return u(this.#call("getWebSocketConnectTimeoutMs", ...args));
	}
	getHideThinkingBlock(...args) {
		return u(this.#call("getHideThinkingBlock", ...args));
	}
	getShowCacheMissNotices(...args) {
		return u(this.#call("getShowCacheMissNotices", ...args));
	}
	getExternalEditorCommand(...args) {
		return u(this.#call("getExternalEditorCommand", ...args));
	}
	setHideThinkingBlock(...args) {
		this.#call("setHideThinkingBlock", ...args);
	}
	setShowCacheMissNotices(...args) {
		this.#call("setShowCacheMissNotices", ...args);
	}
	getShellPath(...args) {
		return u(this.#call("getShellPath", ...args));
	}
	setShellPath(...args) {
		this.#call("setShellPath", ...args);
	}
	getQuietStartup(...args) {
		return u(this.#call("getQuietStartup", ...args));
	}
	setQuietStartup(...args) {
		this.#call("setQuietStartup", ...args);
	}
	getDefaultProjectTrust(...args) {
		return u(this.#call("getDefaultProjectTrust", ...args));
	}
	setDefaultProjectTrust(...args) {
		this.#call("setDefaultProjectTrust", ...args);
	}
	getShellCommandPrefix(...args) {
		return u(this.#call("getShellCommandPrefix", ...args));
	}
	setShellCommandPrefix(...args) {
		this.#call("setShellCommandPrefix", ...args);
	}
	getNpmCommand(...args) {
		return u(this.#call("getNpmCommand", ...args));
	}
	setNpmCommand(...args) {
		this.#call("setNpmCommand", ...args);
	}
	getCollapseChangelog(...args) {
		return u(this.#call("getCollapseChangelog", ...args));
	}
	setCollapseChangelog(...args) {
		this.#call("setCollapseChangelog", ...args);
	}
	getEnableInstallTelemetry(...args) {
		return u(this.#call("getEnableInstallTelemetry", ...args));
	}
	setEnableInstallTelemetry(...args) {
		this.#call("setEnableInstallTelemetry", ...args);
	}
	getEnableAnalytics(...args) {
		return u(this.#call("getEnableAnalytics", ...args));
	}
	getTrackingId(...args) {
		return u(this.#call("getTrackingId", ...args));
	}
	setEnableAnalytics(...args) {
		this.#call("setEnableAnalytics", ...args);
	}
	getOrCreateDeviceId(...args) {
		return u(this.#call("getOrCreateDeviceId", ...args));
	}
	getPackages(...args) {
		return u(this.#call("getPackages", ...args));
	}
	setPackages(...args) {
		this.#call("setPackages", ...args);
	}
	setProjectPackages(...args) {
		this.#call("setProjectPackages", ...args);
	}
	getExtensionPaths(...args) {
		return u(this.#call("getExtensionPaths", ...args));
	}
	setExtensionPaths(...args) {
		this.#call("setExtensionPaths", ...args);
	}
	setProjectExtensionPaths(...args) {
		this.#call("setProjectExtensionPaths", ...args);
	}
	getSkillPaths(...args) {
		return u(this.#call("getSkillPaths", ...args));
	}
	setSkillPaths(...args) {
		this.#call("setSkillPaths", ...args);
	}
	setProjectSkillPaths(...args) {
		this.#call("setProjectSkillPaths", ...args);
	}
	getPromptTemplatePaths(...args) {
		return u(this.#call("getPromptTemplatePaths", ...args));
	}
	setPromptTemplatePaths(...args) {
		this.#call("setPromptTemplatePaths", ...args);
	}
	setProjectPromptTemplatePaths(...args) {
		this.#call("setProjectPromptTemplatePaths", ...args);
	}
	getThemePaths(...args) {
		return u(this.#call("getThemePaths", ...args));
	}
	setThemePaths(...args) {
		this.#call("setThemePaths", ...args);
	}
	setProjectThemePaths(...args) {
		this.#call("setProjectThemePaths", ...args);
	}
	getEnableSkillCommands(...args) {
		return u(this.#call("getEnableSkillCommands", ...args));
	}
	setEnableSkillCommands(...args) {
		this.#call("setEnableSkillCommands", ...args);
	}
	getThinkingBudgets(...args) {
		return u(this.#call("getThinkingBudgets", ...args));
	}
	getTerminalCapabilityOverrides(...args) {
		return u(this.#call("getTerminalCapabilityOverrides", ...args));
	}
	getShowImages(...args) {
		return u(this.#call("getShowImages", ...args));
	}
	setShowImages(...args) {
		this.#call("setShowImages", ...args);
	}
	getImageWidthCells(...args) {
		return u(this.#call("getImageWidthCells", ...args));
	}
	setImageWidthCells(...args) {
		this.#call("setImageWidthCells", ...args);
	}
	getClearOnShrink(...args) {
		return u(this.#call("getClearOnShrink", ...args));
	}
	setClearOnShrink(...args) {
		this.#call("setClearOnShrink", ...args);
	}
	getShowTerminalProgress(...args) {
		return u(this.#call("getShowTerminalProgress", ...args));
	}
	setShowTerminalProgress(...args) {
		this.#call("setShowTerminalProgress", ...args);
	}
	getTuiMode(...args) {
		return u(this.#call("getTuiMode", ...args));
	}
	setTuiMode(...args) {
		this.#call("setTuiMode", ...args);
	}
	getFullscreenExitOutput(...args) {
		return u(this.#call("getFullscreenExitOutput", ...args));
	}
	setFullscreenExitOutput(...args) {
		this.#call("setFullscreenExitOutput", ...args);
	}
	getFullscreenScrollbar(...args) {
		return u(this.#call("getFullscreenScrollbar", ...args));
	}
	setFullscreenScrollbar(...args) {
		this.#call("setFullscreenScrollbar", ...args);
	}
	getFullscreenCopyOnSelect(...args) {
		return u(this.#call("getFullscreenCopyOnSelect", ...args));
	}
	setFullscreenCopyOnSelect(...args) {
		this.#call("setFullscreenCopyOnSelect", ...args);
	}
	getFullscreenWheelScrollLines(...args) {
		return u(this.#call("getFullscreenWheelScrollLines", ...args));
	}
	setFullscreenWheelScrollLines(...args) {
		this.#call("setFullscreenWheelScrollLines", ...args);
	}
	getImageAutoResize(...args) {
		return u(this.#call("getImageAutoResize", ...args));
	}
	setImageAutoResize(...args) {
		this.#call("setImageAutoResize", ...args);
	}
	getBlockImages(...args) {
		return u(this.#call("getBlockImages", ...args));
	}
	setBlockImages(...args) {
		this.#call("setBlockImages", ...args);
	}
	getEnabledModels(...args) {
		return u(this.#call("getEnabledModels", ...args));
	}
	getDefaultTools(...args) {
		return u(this.#call("getDefaultTools", ...args));
	}
	setEnabledModels(...args) {
		this.#call("setEnabledModels", ...args);
	}
	getDoubleEscapeAction(...args) {
		return u(this.#call("getDoubleEscapeAction", ...args));
	}
	setDoubleEscapeAction(...args) {
		this.#call("setDoubleEscapeAction", ...args);
	}
	getTreeFilterMode(...args) {
		return u(this.#call("getTreeFilterMode", ...args));
	}
	setTreeFilterMode(...args) {
		this.#call("setTreeFilterMode", ...args);
	}
	getShowHardwareCursor(...args) {
		return u(this.#call("getShowHardwareCursor", ...args));
	}
	setShowHardwareCursor(...args) {
		this.#call("setShowHardwareCursor", ...args);
	}
	getEditorPaddingX(...args) {
		return u(this.#call("getEditorPaddingX", ...args));
	}
	setEditorPaddingX(...args) {
		this.#call("setEditorPaddingX", ...args);
	}
	getOutputPad(...args) {
		return u(this.#call("getOutputPad", ...args));
	}
	setOutputPad(...args) {
		this.#call("setOutputPad", ...args);
	}
	getAutocompleteMaxVisible(...args) {
		return u(this.#call("getAutocompleteMaxVisible", ...args));
	}
	setAutocompleteMaxVisible(...args) {
		this.#call("setAutocompleteMaxVisible", ...args);
	}
	getCodeBlockIndent(...args) {
		return u(this.#call("getCodeBlockIndent", ...args));
	}
	getMermaidRenderingMode(...args) {
		return u(this.#call("getMermaidRenderingMode", ...args));
	}
	setMermaidRenderingMode(...args) {
		this.#call("setMermaidRenderingMode", ...args);
	}
	getWarnings(...args) {
		return u(this.#call("getWarnings", ...args));
	}
	setWarnings(...args) {
		this.#call("setWarnings", ...args);
	}
}

// ---------------------------------------------------------------------------
// ModelRuntime
// ---------------------------------------------------------------------------

const runtimeFinalizer =
	typeof FinalizationRegistry === "function"
		? new FinalizationRegistry((id) => {
				try {
					call("rt", { runtime: id, m: "release", a: [] });
				} catch {}
			})
		: undefined;

const signalOption = (options) =>
	options && typeof options === "object" ? { ...options, signal: tokenFor(options.signal) } : (options ?? null);
const providerFromHost = (value) => globalThis.__piAiRust.providerFromHost(value);

/** TS `ModelRuntime` over a Rust model runtime (`ModelRuntime.create()`, `ctx.modelRegistry.runtime`). */
export class ModelRuntime {
	#id;

	constructor(token) {
		if (!token || typeof token !== "object" || !(SDK_OBJECT in token)) {
			throw new TypeError("ModelRuntime is created with ModelRuntime.create()");
		}
		this.#id = token[SDK_OBJECT];
		if (token.owned) runtimeFinalizer?.register(this, this.#id);
	}

	/** @internal The Rust runtime id (SDK options). */
	get __piRuntimeId() {
		return this.#id;
	}

	static async create(options = {}) {
		const id = await callAsync("rt.async", { m: "create", a: [{ ...options, signal: tokenFor(options.signal) }] });
		const runtime = new ModelRuntime({ [SDK_OBJECT]: id, owned: true });
		rememberRuntime(id, runtime);
		return runtime;
	}

	#call(m, ...a) {
		return u(call("rt", { runtime: this.#id, m, a: a.map((v) => (v === undefined ? null : v)) }));
	}

	async #async(m, ...a) {
		return u(await callAsync("rt.async", { runtime: this.#id, m, a: a.map((v) => (v === undefined ? null : v)) }));
	}

	#stream(m, model, second, options) {
		return streamFromHost("rt.stream", { runtime: this.#id, m, a: [model, second, signalOption(options)] });
	}

	getProviders() {
		return (this.#call("getProviders") ?? []).map(providerFromHost);
	}
	getProvider(providerId) {
		return providerFromHost(this.#call("getProvider", providerId));
	}
	getModels(providerId) {
		return this.#call("getModels", providerId) ?? [];
	}
	getModel(providerId, modelId) {
		return this.#call("getModel", providerId, modelId);
	}
	getModelsOfType(type, providerId) {
		return this.#call("getModelsOfType", type, providerId) ?? [];
	}
	getModelOfType(type, providerId, modelId) {
		return this.#call("getModelOfType", type, providerId, modelId);
	}
	getAllModels(providerId) {
		return this.#call("getAllModels", providerId) ?? [];
	}
	async getAvailableOfType(type, providerId, options) {
		return (await this.#async("getAvailableOfType", type, providerId, signalOption(options))) ?? [];
	}
	async getAllAvailable(providerId, options) {
		return (await this.#async("getAllAvailable", providerId, signalOption(options))) ?? [];
	}
	checkAuth(providerId, options) {
		return this.#async("checkAuth", providerId, signalOption(options));
	}
	async getAvailable(providerId, options) {
		return (await this.#async("getAvailable", providerId, signalOption(options))) ?? [];
	}
	getAvailableSnapshot() {
		return this.#call("getAvailableSnapshot") ?? [];
	}
	getError() {
		return this.#call("getError");
	}
	getRegisteredProviderConfig(providerId) {
		return globalThis.__piAiRust.providerConfigFromHost(providerId, this.#call("getRegisteredProviderConfig", providerId));
	}
	getRegisteredProviderIds() {
		return this.#call("getRegisteredProviderIds") ?? [];
	}
	getRegisteredNativeProvider(providerId) {
		return providerFromHost(this.#call("getRegisteredNativeProvider", providerId));
	}
	getCompatibilityRequestConfig(model) {
		return this.#call("getCompatibilityRequestConfig", model);
	}
	isUsingOAuth(providerId) {
		return !!this.#call("isUsingOAuth", providerId);
	}
	isUsingSubscription(providerId) {
		return !!this.#call("isUsingSubscription", providerId);
	}
	hasConfiguredAuth(providerId) {
		return !!this.#call("hasConfiguredAuth", providerId);
	}
	getAuth(providerOrModel, overrides) {
		return this.#async("getAuth", providerOrModel, signalOption(overrides));
	}
	async setRuntimeApiKey(providerId, apiKey, options) {
		await this.#async("setRuntimeApiKey", providerId, apiKey, signalOption(options));
	}
	async removeRuntimeApiKey(providerId, options) {
		await this.#async("removeRuntimeApiKey", providerId, signalOption(options));
	}
	async listCredentials(options) {
		return (await this.#async("listCredentials", signalOption(options))) ?? [];
	}
	getProviderAuthStatus(providerId) {
		return this.#call("getProviderAuthStatus", providerId);
	}
	stream(model, context, options) {
		return this.#stream("stream", model, context, options);
	}
	complete(model, context, options) {
		return this.#async("complete", model, context, signalOption(options));
	}
	streamSimple(model, context, options) {
		return this.#stream("streamSimple", model, context, options);
	}
	completeSimple(model, context, options) {
		return this.#async("completeSimple", model, context, signalOption(options));
	}
	streamDeferred(model, handle, options) {
		return this.#stream("streamDeferred", model, handle, options);
	}
	fetchDeferred(model, handle, options) {
		return this.#async("fetchDeferred", model, handle, signalOption(options));
	}
	async cancelDeferred(model, handle, options) {
		await this.#async("cancelDeferred", model, handle, signalOption(options));
	}
	generateImages(model, context, options) {
		return this.#async("generateImages", model, context, signalOption(options));
	}
	classify(model, context, options) {
		return this.#async("classify", model, context, signalOption(options));
	}
	async login(providerId, type, interaction, options) {
		return u(
			await callAsync("rt.async", {
				runtime: this.#id,
				m: "login",
				a: [providerId, type, ref(interaction), options ?? null],
				signal: tokenFor(interaction?.signal),
			}),
		);
	}
	async logout(providerId, options) {
		await this.#async("logout", providerId, signalOption(options));
	}
	async refresh(options) {
		const result = await this.#async("refresh", signalOption(options ?? {}));
		return {
			aborted: !!result?.aborted,
			errors: new Map(Object.entries(result?.errors ?? {}).map(([provider, message]) => [provider, new Error(message)])),
		};
	}
	registerNativeProvider(provider) {
		this.#call("registerNativeProvider", describeNativeProvider(provider));
	}
	registerProvider(providerId, config) {
		this.#call("registerProvider", providerId, describeProviderConfig(config));
	}
	unregisterProvider(providerId) {
		this.#call("unregisterProvider", providerId);
	}
	registerVirtualModel(definition) {
		this.#call("registerVirtualModel", definition);
	}
	unregisterVirtualModel(providerId, id) {
		this.#call("unregisterVirtualModel", providerId, id);
	}
	resolveModel(model, messages, options) {
		return this.#async("resolveModel", model, messages, signalOption(options));
	}
	getPhysicalModel(providerId, modelId) {
		return this.#call("getPhysicalModel", providerId, modelId);
	}
}

/** Runtimes by id, so `registry.runtime` is one object per runtime (created ones held weakly). */
const knownRuntimes = new Map();

function rememberRuntime(id, runtime) {
	knownRuntimes.set(id, typeof WeakRef === "function" ? new WeakRef(runtime) : runtime);
}

function runtimeFor(id) {
	const known = knownRuntimes.get(id);
	let runtime = known instanceof ModelRuntime ? known : known?.deref?.();
	if (!runtime) {
		// Host runtimes (`ctx.modelRegistry.runtime`) live as long as pi does: keep them strongly.
		runtime = new ModelRuntime({ [SDK_OBJECT]: id });
		knownRuntimes.set(id, runtime);
	}
	return runtime;
}

globalThis.__piSdk = { runtimeFor, extensionsResultFromHost, extensionsResultToHost };

export { ModelRegistry };

// ---------------------------------------------------------------------------
// Rust tools as JavaScript tool definitions
// ---------------------------------------------------------------------------

/**
 * A tool definition from Rust (`describe_definition`): the extension's own object, or a tool
 * object whose functions call the Rust tool. `withRenderers` adds prepareArguments/renderers.
 */
export function toolFromHost(d, withRenderers = true) {
	if (!d) return undefined;
	if (d.object !== undefined) return d.object;
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
	if (d.defaultActive !== undefined) tool.defaultActive = d.defaultActive;
	if (d.exposure !== undefined) tool.exposure = d.exposure;
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
	if (withRenderers) {
		if (d.hasPrepareArguments) {
			const prepareArguments = (args) => u(call("ca", { fn: "builtinPrepareArguments", args: [id, args] }));
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

// ---------------------------------------------------------------------------
// Built-in extension factories (Rust implementations)
// ---------------------------------------------------------------------------

function builtinExtension(name, options) {
	const factory = async (pi) => {
		if (!pi || typeof pi.__piApiId !== "number") {
			throw new Error(`The built-in ${name} extension factory expects the pi object of an extension`);
		}
		await callAsync("builtin.ext.run", { api: pi.__piApiId, name, options: options ?? null });
	};
	Object.defineProperty(factory, "__piBuiltinExtension", { value: { name, options: options ?? null } });
	return factory;
}

export const createCodemodeExtension = (options) => builtinExtension("codemode", options);
export const createToolSearchExtension = () => builtinExtension("tool-search");
export const createMcpExtension = (options) => {
	const data = {};
	if (options?.logPath !== undefined) data.logPath = options.logPath;
	return builtinExtension("mcp", data);
};

/** An extension factory for Rust: built-in markers stay markers; functions become handles. */
function describeFactory(factory) {
	const builtin = factory?.__piBuiltinExtension;
	if (builtin) return { $piBuiltinExtension: builtin.name, options: builtin.options };
	if (typeof factory !== "function") throw new TypeError("extension factory must be a function");
	return factory;
}

// ---------------------------------------------------------------------------
// DefaultResourceLoader
// ---------------------------------------------------------------------------

function mapOf(record, value = (v) => v) {
	return new Map(Object.entries(record ?? {}).map(([k, v]) => [k, value(v, k)]));
}

/** A `LoadExtensionsResult` from Rust (`extensions_result_to_js`). */
/** `RegisteredTool` of a loaded extension; `definition` (with `execute`) is fetched when first read. */
function registeredToolFromHost(tool, name, extensionId) {
	let definition;
	return {
		get definition() {
			if (definition === undefined) {
				definition = extensionId === undefined || extensionId === null ? null : toolFromHost(call("sdk.extensionTool", { ext: extensionId, a: [name] }));
				definition ??= tool.definition;
			}
			return definition;
		},
		sourceInfo: tool.sourceInfo,
	};
}

export function extensionsResultFromHost(result) {
	const extensions = (result?.extensions ?? []).map((e) => {
		const extension = {
			path: e.path,
			resolvedPath: e.resolvedPath,
			sourceInfo: e.sourceInfo,
			handlers: mapOf(e.handlers, (count) => Array.from({ length: count }, () => () => undefined)),
			tools: mapOf(e.tools, (tool, name) => registeredToolFromHost(tool, name, e.$extension)),
			messageRenderers: new Map((e.messageRenderers ?? []).map((name) => [name, () => undefined])),
			commands: mapOf(e.commands),
			flags: mapOf(e.flags),
			shortcuts: mapOf(e.shortcuts),
		};
		if (e.hidden !== undefined) extension.hidden = e.hidden;
		if (e.replaceable !== undefined) extension.replaceable = e.replaceable;
		Object.defineProperty(extension, "__piIndex", { value: e.$index, enumerable: false });
		Object.defineProperty(extension, "__piExtension", { value: e.$extension, enumerable: false });
		return extension;
	});
	const out = { extensions, errors: result?.errors ?? [], runtime: extensionRuntimeFor(result?.$runtime) };
	if (result?.warnings) out.warnings = result.warnings;
	return out;
}

/** `ExtensionRuntime` objects by id (`createExtensionRuntime()`, `LoadExtensionsResult.runtime`). */
const extensionRuntimes = new Map();

function extensionRuntimeFor(id) {
	if (id === undefined || id === null) return {};
	let runtime = extensionRuntimes.get(id);
	if (!runtime) {
		runtime = { flagValues: new Map(), pendingProviderRegistrations: [] };
		Object.defineProperty(runtime, "__piExtRuntime", { value: id, enumerable: false });
		extensionRuntimes.set(id, runtime);
	}
	return runtime;
}

/** TS `createExtensionRuntime()`: a fresh extension runtime (for ResourceLoader implementations). */
export function createExtensionRuntime() {
	return extensionRuntimeFor(call("sdk.createExtensionRuntime", {}));
}

/** TS `discoverAndLoadExtensions(paths, cwd, agentDir, eventBus)`, loaded by pi's Rust loader. */
export async function discoverAndLoadExtensions(configuredPaths, cwd, agentDir, _eventBus) {
	return extensionsResultFromHost(
		await callAsync("sdk.discoverAndLoadExtensions", { a: [configuredPaths ?? [], cwd ?? process.cwd(), agentDir ?? null] }),
	);
}

/** A `LoadExtensionsResult` for Rust: extensions and the runtime by their pi identity. */
export function extensionsResultToHost(result) {
	const out = {
		extensions: (result?.extensions ?? []).map((e) => ({ $extension: e?.__piExtension ?? null })),
		errors: result?.errors ?? [],
	};
	if (result?.warnings) out.warnings = result.warnings;
	if (result?.runtime?.__piExtRuntime !== undefined) out.$runtime = result.runtime.__piExtRuntime;
	return out;
}

/** The value Rust receives for a `resourceLoader` option: a DefaultResourceLoader or any object. */
function describeLoader(loader) {
	const id = loader?.__piLoaderId;
	return id !== undefined ? { $loader: id } : { object: ref(loader) };
}

// ---------------------------------------------------------------------------
// ProjectTrustStore, DefaultPackageManager, model resolution
// ---------------------------------------------------------------------------

/** TS `ProjectTrustStore` (the trust decisions in `<agentDir>/trust.json`). */
export class ProjectTrustStore {
	#agentDir;
	constructor(agentDir) {
		this.#agentDir = agentDir;
	}
	#call(m, ...a) {
		return call("trust", { agentDir: this.#agentDir, m, a: a.map((v) => (v === undefined ? null : v)) });
	}
	/** `true`, `false`, or `null` when undecided. */
	get(cwd) {
		return this.#call("get", cwd);
	}
	getEntry(cwd) {
		return this.#call("getEntry", cwd) ?? null;
	}
	set(cwd, decision) {
		this.#call("set", cwd, decision);
	}
	setMany(decisions) {
		this.#call("setMany", decisions ?? []);
	}
}

const packageManagerFinalizer = finalizer("pm");

/** TS `DefaultPackageManager` over the Rust package manager. */
export class DefaultPackageManager {
	#id;
	constructor(options = {}) {
		this.#id = call("pm", {
			m: "new",
			a: [
				{
					cwd: options.cwd,
					agentDir: options.agentDir,
					settingsManager: options.settingsManager?.__piSettingsId ?? null,
					builtinExtensions: options.builtinExtensions,
				},
			],
		});
		packageManagerFinalizer?.register(this, this.#id);
	}
	#call(m, ...a) {
		return u(call("pm", { id: this.#id, m, a: a.map((v) => (v === undefined ? null : v)) }));
	}
	async #async(m, ...a) {
		return u(await callAsync("pm.async", { id: this.#id, m, a: a.map((v) => (v === undefined ? null : v)) }));
	}
	setProgressCallback(callback) {
		this.#call("setProgressCallback", typeof callback === "function" ? callback : null);
	}
	addSourceToSettings(source, options) {
		return this.#call("addSourceToSettings", source, options);
	}
	removeSourceFromSettings(source, options) {
		return this.#call("removeSourceFromSettings", source, options);
	}
	getInstalledPath(source, scope) {
		return this.#call("getInstalledPath", source, scope);
	}
	resolve(onMissing) {
		return this.#async("resolve", typeof onMissing === "function" ? onMissing : null);
	}
	resolveExtensionSources(sources, options) {
		return this.#async("resolveExtensionSources", sources, options);
	}
	listConfiguredPackages() {
		return this.#call("listConfiguredPackages") ?? [];
	}
	async install(source, options) {
		await this.#async("install", source, options);
	}
	async installAndPersist(source, options) {
		await this.#async("installAndPersist", source, options);
	}
	async remove(source, options) {
		await this.#async("remove", source, options);
	}
	removeAndPersist(source, options) {
		return this.#async("removeAndPersist", source, options);
	}
	async update(source) {
		await this.#async("update", source);
	}
	async checkForAvailableUpdates() {
		return (await this.#async("checkForAvailableUpdates")) ?? [];
	}
}

/** TS `resolveCliModel({ cliProvider, cliModel, cliThinking, inlineApiKey, modelRuntime })`. */
export function resolveCliModel(options = {}) {
	const result = call("sdk.resolveCliModel", {
		a: [
			{
				cliProvider: options.cliProvider,
				cliModel: options.cliModel,
				cliThinking: options.cliThinking,
				inlineApiKey: options.inlineApiKey,
				modelRuntime: options.modelRuntime?.__piRuntimeId ?? null,
			},
		],
	});
	return { model: result?.model, thinkingLevel: result?.thinkingLevel, warning: result?.warning, error: result?.error };
}

/** TS `resolveModelScopeWithDiagnostics(patterns, modelRuntime, options)`. */
export async function resolveModelScopeWithDiagnostics(patterns, modelRuntime, options) {
	return await callAsync("sdk.resolveModelScope", {
		a: [patterns ?? [], modelRuntime?.__piRuntimeId ?? null, options?.signal ? { signal: tokenFor(options.signal) } : null],
	});
}

const loaderFinalizer = finalizer("rl");

/** TS `DefaultResourceLoader` over the Rust loader. */
export class DefaultResourceLoader {
	#id;

	constructor(options = {}) {
		const o = { ...options };
		if (o.settingsManager !== undefined) o.settingsManager = o.settingsManager?.__piSettingsId ?? null;
		if (o.eventBus !== undefined) delete o.eventBus;
		if (o.extensionFactories) {
			o.extensionFactories = o.extensionFactories.map((entry) =>
				typeof entry === "function"
					? describeFactory(entry)
					: { ...entry, factory: describeFactory(entry.factory) },
			);
		}
		for (const key of Object.keys(o)) if (o[key] === undefined) delete o[key];
		this.#id = call("rl", { m: "new", a: [o] });
		loaderFinalizer?.register(this, this.#id);
	}

	/** @internal The Rust loader id (SDK options). */
	get __piLoaderId() {
		return this.#id;
	}

	#call(m, ...a) {
		return u(call("rl", { id: this.#id, m, a }));
	}

	async reload(options) {
		const resolveProjectTrust = typeof options?.resolveProjectTrust === "function" ? options.resolveProjectTrust : undefined;
		await callAsync("rl.async", resolveProjectTrust ? { id: this.#id, resolveProjectTrust } : { id: this.#id });
	}
	getExtensions() {
		return extensionsResultFromHost(this.#call("getExtensions"));
	}
	getSkills() {
		return this.#call("getSkills");
	}
	getPrompts() {
		return this.#call("getPrompts");
	}
	getThemes() {
		return this.#call("getThemes");
	}
	getAgentsFiles() {
		return this.#call("getAgentsFiles");
	}
	getSystemPrompt() {
		return this.#call("getSystemPrompt");
	}
	getAppendSystemPrompt() {
		return this.#call("getAppendSystemPrompt") ?? [];
	}
	extendResources(paths) {
		this.#call("extendResources", paths ?? {});
	}
}

// ---------------------------------------------------------------------------
// createAgentSession and AgentSession
// ---------------------------------------------------------------------------

const sessionFinalizer = finalizer("as");

/** `session.agent`: the parts of pi-agent-core's Agent that extensions reach through a session. */
class SessionAgent {
	#session;
	constructor(session) {
		this.#session = session;
	}
	get state() {
		return this.#session.state;
	}
	get beforeToolCall() {
		return hookFromHost(this.#session.__piCall("agent.getBeforeToolCall"));
	}
	set beforeToolCall(fn) {
		this.#session.__piCall("agent.setBeforeToolCall", typeof fn === "function" ? fn : null);
	}
	get afterToolCall() {
		return hookFromHost(this.#session.__piCall("agent.getAfterToolCall"));
	}
	set afterToolCall(fn) {
		this.#session.__piCall("agent.setAfterToolCall", typeof fn === "function" ? fn : null);
	}
}

function hookFromHost(value) {
	if (!value) return undefined;
	if (value.object !== undefined) return value.object;
	const hook = value.native;
	return async (context, signal) => {
		// The Rust context of the hook call in progress travels as `$hookContext` (non-enumerable).
		const ctx = context && context.$hookContext !== undefined ? { ...context, $hookContext: context.$hookContext } : context;
		return u(await callAsync("as.async", { id: value.session, m: "agent.callHook", a: [hook, ctx], signal: tokenFor(signal) }));
	};
}

/** TS `AgentSession` over a Rust agent session (`createAgentSession()`). */
/** `session.extensionRunner`: event dispatch to the session's extensions. */
class SessionExtensionRunner {
	#session;

	constructor(session) {
		this.#session = session;
	}
	hasHandlers(eventType) {
		return !!this.#session.__piCall("runner.hasHandlers", eventType);
	}
	async emit(event) {
		return await this.#session.__piAsync("runner.emit", event);
	}
	createContext() {
		return makeContext(this.#session.__piCall("runner.createContext"), "base");
	}
	createCommandContext() {
		return makeContext(this.#session.__piCall("runner.createCommandContext"), "command");
	}
	createToolContext(toolCallId, signal) {
		return makeContext(this.#session.__piCall("runner.createToolContext", toolCallId, tokenFor(signal) ?? null), "tool");
	}
}

/** TS `wrapRegisteredTool(registeredTool, runner)`: an AgentTool running the definition with the runner's tool context. */
export function wrapRegisteredTool(registeredTool, runner) {
	const definition = registeredTool.definition;
	return {
		name: definition.name,
		label: definition.label,
		description: definition.description,
		parameters: definition.parameters,
		outputSchema: definition.outputSchema,
		constrainedSampling: definition.constrainedSampling,
		prepareArguments: definition.prepareArguments,
		executionMode: definition.executionMode,
		execute: (toolCallId, params, signal, onUpdate, ctx) =>
			definition.execute(toolCallId, params, signal, onUpdate, ctx ?? runner.createToolContext(toolCallId, signal)),
	};
}

/** TS `wrapRegisteredTools(registeredTools, runner)`. */
export function wrapRegisteredTools(registeredTools, runner) {
	return registeredTools.map((tool) => wrapRegisteredTool(tool, runner));
}

export class AgentSession {
	#id;
	#loader;
	#agent;
	#runner;

	constructor(token) {
		if (!token || typeof token !== "object" || !(SDK_OBJECT in token)) {
			throw new TypeError("AgentSession is created with createAgentSession()");
		}
		this.#id = token[SDK_OBJECT];
		this.#loader = token.loader;
		sessionFinalizer?.register(this, this.#id);
	}

	/** @internal */
	__piCall(m, ...a) {
		return u(call("as", { id: this.#id, m, a: a.map((v) => (v === undefined ? null : v)) }));
	}

	async #async(m, ...a) {
		return u(await callAsync("as.async", { id: this.#id, m, a: a.map((v) => (v === undefined ? null : v)) }));
	}

	/** @internal */
	__piAsync(m, ...a) {
		return this.#async(m, ...a);
	}

	get extensionRunner() {
		this.#runner ??= new SessionExtensionRunner(this);
		return this.#runner;
	}
	createReplacedSessionContext() {
		return makeContext(this.__piCall("createReplacedSessionContext"), "replaced");
	}

	get agent() {
		this.#agent ??= new SessionAgent(this);
		return this.#agent;
	}
	get sessionManager() {
		return globalThis.__piSessionManagerFor(this.__piCall("sessionManager"));
	}
	get settingsManager() {
		return SettingsManager.__host(this.__piCall("settingsManager"));
	}
	get modelRuntime() {
		return runtimeFor(this.__piCall("modelRuntime"));
	}
	get resourceLoader() {
		return this.#loader;
	}
	subscribe(listener) {
		const id = this.__piCall("subscribe", listener);
		return () => call("unsub", { id });
	}
	dispose() {
		this.__piCall("dispose");
	}
	refreshContext() {
		this.__piCall("refreshContext");
	}
	get state() {
		const state = this.__piCall("state");
		return state ? { ...state, pendingToolCalls: new Set(state.pendingToolCalls ?? []) } : state;
	}
	get cacheWarmingStatus() {
		return this.__piCall("cacheWarmingStatus");
	}
	setCacheWarmingMode(mode) {
		this.__piCall("setCacheWarmingMode", mode);
	}
	get model() {
		return this.__piCall("model");
	}
	get thinkingLevel() {
		return this.__piCall("thinkingLevel");
	}
	get routedModel() {
		return this.__piCall("routedModel");
	}
	get isStreaming() {
		return !!this.__piCall("isStreaming");
	}
	get isIdle() {
		return !!this.__piCall("isIdle");
	}
	get systemPrompt() {
		return this.__piCall("systemPrompt") ?? "";
	}
	get retryAttempt() {
		return this.__piCall("retryAttempt") ?? 0;
	}
	getActiveToolNames() {
		return this.__piCall("getActiveToolNames") ?? [];
	}
	getCallableToolNames() {
		return this.__piCall("getCallableToolNames") ?? [];
	}
	getAllTools() {
		return this.__piCall("getAllTools") ?? [];
	}
	getToolDefinition(name) {
		return toolFromHost(this.__piCall("getToolDefinition", name));
	}
	setActiveToolsByName(toolNames) {
		this.__piCall("setActiveToolsByName", toolNames ?? []);
	}
	get isCompacting() {
		return !!this.__piCall("isCompacting");
	}
	get messages() {
		return this.__piCall("messages") ?? [];
	}
	get steeringMode() {
		return this.__piCall("steeringMode");
	}
	get followUpMode() {
		return this.__piCall("followUpMode");
	}
	get sessionFile() {
		return this.__piCall("sessionFile");
	}
	get sessionId() {
		return this.__piCall("sessionId");
	}
	get sessionName() {
		return this.__piCall("sessionName");
	}
	get scopedModels() {
		return this.__piCall("scopedModels") ?? [];
	}
	setScopedModels(scopedModels) {
		this.__piCall("setScopedModels", scopedModels ?? []);
	}
	get promptTemplates() {
		return this.__piCall("promptTemplates") ?? [];
	}
	async prompt(text, options) {
		await this.#async("prompt", text, options ?? {});
	}
	steer(text, images, options) {
		return this.#async("steer", text, images, options);
	}
	followUp(text, images, options) {
		return this.#async("followUp", text, images, options);
	}
	async sendCustomMessage(message, options) {
		await this.#async("sendCustomMessage", message, options);
	}
	async sendUserMessage(content, options) {
		await this.#async("sendUserMessage", content, options);
	}
	clearQueue() {
		return this.__piCall("clearQueue");
	}
	get pendingMessageCount() {
		return this.__piCall("pendingMessageCount") ?? 0;
	}
	getSteeringMessages() {
		return this.__piCall("getSteeringMessages") ?? [];
	}
	getFollowUpMessages() {
		return this.__piCall("getFollowUpMessages") ?? [];
	}
	async abort() {
		await this.#async("abort");
	}
	async waitForIdle() {
		await this.#async("waitForIdle");
	}
	async setModel(model, options) {
		await this.#async("setModel", model, options);
	}
	cycleModel(direction, options) {
		return this.#async("cycleModel", direction, options);
	}
	setThinkingLevel(level, options) {
		this.__piCall("setThinkingLevel", level, options);
	}
	cycleThinkingLevel(options) {
		return this.__piCall("cycleThinkingLevel", options);
	}
	getAvailableThinkingLevels() {
		return this.__piCall("getAvailableThinkingLevels") ?? [];
	}
	supportsThinking() {
		return !!this.__piCall("supportsThinking");
	}
	setSteeringMode(mode) {
		this.__piCall("setSteeringMode", mode);
	}
	setFollowUpMode(mode) {
		this.__piCall("setFollowUpMode", mode);
	}
	compact(customInstructions) {
		return this.#async("compact", customInstructions);
	}
	abortCompaction() {
		this.__piCall("abortCompaction");
	}
	abortBranchSummary() {
		this.__piCall("abortBranchSummary");
	}
	setAutoCompactionEnabled(enabled) {
		this.__piCall("setAutoCompactionEnabled", !!enabled);
	}
	get autoCompactionEnabled() {
		return !!this.__piCall("autoCompactionEnabled");
	}
	async bindExtensions(bindings = {}) {
		await this.#async("bindExtensions", {
			mode: bindings.mode,
			onError: bindings.onError,
			shutdownHandler: bindings.shutdownHandler,
			abortHandler: bindings.abortHandler,
		});
	}
	async reload() {
		await this.#async("reload");
	}
	abortRetry() {
		this.__piCall("abortRetry");
	}
	get isRetrying() {
		return !!this.__piCall("isRetrying");
	}
	get autoRetryEnabled() {
		return !!this.__piCall("autoRetryEnabled");
	}
	setAutoRetryEnabled(enabled) {
		this.__piCall("setAutoRetryEnabled", !!enabled);
	}
	recordBashResult(command, result, options) {
		this.__piCall("recordBashResult", command, result, options);
	}
	abortBash() {
		this.__piCall("abortBash");
	}
	get isBashRunning() {
		return !!this.__piCall("isBashRunning");
	}
	get hasPendingBashMessages() {
		return !!this.__piCall("hasPendingBashMessages");
	}
	setSessionName(name) {
		this.__piCall("setSessionName", name);
	}
	navigateTree(targetId, options) {
		return this.#async("navigateTree", targetId, options);
	}
	getUserMessagesForForking() {
		return this.__piCall("getUserMessagesForForking") ?? [];
	}
	getSessionStats() {
		return this.__piCall("getSessionStats");
	}
	getContextUsage() {
		return this.__piCall("getContextUsage");
	}
	exportToHtml(outputPath, options) {
		return this.#async("exportToHtml", outputPath, options);
	}
	exportToJsonl(outputPath) {
		return this.__piCall("exportToJsonl", outputPath);
	}
	getLastAssistantText() {
		return this.__piCall("getLastAssistantText");
	}
	hasExtensionHandlers(eventType) {
		return !!this.__piCall("hasExtensionHandlers", eventType);
	}
}

/** TS `createAgentSession(options)`: a Rust AgentSession driven from JavaScript. */
export async function createAgentSession(options = {}) {
	const o = {};
	for (const key of ["cwd", "agentDir", "model", "thinkingLevel", "scopedModels", "noTools", "tools", "excludeTools", "sessionStartEvent"]) {
		if (options[key] !== undefined) o[key] = options[key];
	}
	if (options.modelRuntime !== undefined) o.modelRuntime = options.modelRuntime?.__piRuntimeId ?? null;
	if (options.modelRegistry !== undefined && o.modelRuntime === undefined) o.modelRegistry = options.modelRegistry?.__piRegistryTarget ?? null;
	if (options.customTools) o.customTools = options.customTools.map(describeTool);
	if (options.resourceLoader !== undefined && options.resourceLoader !== null) o.resourceLoader = describeLoader(options.resourceLoader);
	if (options.sessionManager !== undefined) o.sessionManager = options.sessionManager?.__piSessionManagerId ?? null;
	if (options.settingsManager !== undefined) o.settingsManager = options.settingsManager?.__piSettingsId ?? null;
	const result = await callAsync("as.async", { m: "create", a: [o] });
	const session = new AgentSession({ [SDK_OBJECT]: result.session, loader: options.resourceLoader });
	const out = { session, extensionsResult: extensionsResultFromHost(result.extensionsResult) };
	if (result.modelFallbackMessage !== undefined) out.modelFallbackMessage = result.modelFallbackMessage;
	return out;
}

// ---------------------------------------------------------------------------
// createAgentSessionServices, createAgentSessionFromServices, AgentSessionRuntime
// ---------------------------------------------------------------------------

/** TS `createAgentSessionServices(options)`: cwd-bound runtime services, no session. */
export async function createAgentSessionServices(options) {
	const paths = call("sdk.servicesPaths", { a: [options.cwd, options.agentDir ?? null] });
	const { cwd, agentDir } = paths;
	const modelRuntime =
		options.modelRuntime ??
		(await ModelRuntime.create({ authPath: paths.authPath, modelsPath: paths.modelsPath, signal: options.modelRuntimeSignal }));
	const settingsManager = options.settingsManager ?? SettingsManager.create(cwd, agentDir);
	const resourceLoader = new DefaultResourceLoader({
		...(options.resourceLoaderOptions ?? {}),
		cwd,
		agentDir,
		settingsManager,
	});
	await resourceLoader.reload(options.resourceLoaderReloadOptions);
	const diagnostics = await callAsync("sdk.finishServices", {
		loader: resourceLoader.__piLoaderId,
		runtime: modelRuntime.__piRuntimeId,
		flags: options.extensionFlagValues ? [...options.extensionFlagValues] : null,
	});
	return { cwd, agentDir, modelRuntime, settingsManager, resourceLoader, diagnostics };
}

/** TS `createAgentSessionFromServices(options)`. */
export async function createAgentSessionFromServices(options) {
	return createAgentSession({
		cwd: options.services.cwd,
		agentDir: options.services.agentDir,
		modelRuntime: options.services.modelRuntime,
		settingsManager: options.services.settingsManager,
		resourceLoader: options.services.resourceLoader,
		sessionManager: options.sessionManager,
		model: options.model,
		thinkingLevel: options.thinkingLevel,
		scopedModels: options.scopedModels,
		tools: options.tools,
		excludeTools: options.excludeTools,
		noTools: options.noTools,
		customTools: options.customTools,
		sessionStartEvent: options.sessionStartEvent,
	});
}

/** Thrown when /import references a JSONL file path that does not exist. */
export class SessionImportFileNotFoundError extends Error {
	constructor(filePath) {
		super(`File not found: ${filePath}`);
		this.name = "SessionImportFileNotFoundError";
		this.filePath = filePath;
	}
}

class MissingSessionCwdError extends Error {
	constructor(issue) {
		const sessionFile = issue.sessionFile ? `\nSession file: ${issue.sessionFile}` : "";
		super(`Stored session working directory does not exist: ${issue.sessionCwd}${sessionFile}\nCurrent working directory: ${issue.fallbackCwd}`);
		this.name = "MissingSessionCwdError";
		this.issue = issue;
	}
}

async function assertSessionCwdExists(sessionManager, fallbackCwd) {
	const sessionFile = sessionManager.getSessionFile();
	if (!sessionFile) return;
	const sessionCwd = sessionManager.getCwd();
	const { existsSync } = await import("node:fs");
	if (!sessionCwd || existsSync(sessionCwd)) return;
	throw new MissingSessionCwdError({ sessionFile, sessionCwd, fallbackCwd });
}

async function emitSessionShutdownEvent(runner, event) {
	if (runner.hasHandlers("session_shutdown")) {
		await runner.emit(event);
		return true;
	}
	return false;
}

function extractUserMessageText(content) {
	if (typeof content === "string") return content;
	return content
		.filter((part) => part.type === "text" && typeof part.text === "string")
		.map((part) => part.text)
		.join("");
}

/** TS `AgentSessionRuntime`: owns the current AgentSession plus its cwd-bound services. */
export class AgentSessionRuntime {
	#rebindSession;
	#beforeSessionInvalidate;
	#session;
	#services;
	#createRuntime;
	#diagnostics;
	#modelFallbackMessage;

	constructor(session, services, createRuntime, diagnostics = [], modelFallbackMessage) {
		this.#session = session;
		this.#services = services;
		this.#createRuntime = createRuntime;
		this.#diagnostics = diagnostics;
		this.#modelFallbackMessage = modelFallbackMessage;
	}

	get services() {
		return this.#services;
	}
	get session() {
		return this.#session;
	}
	get cwd() {
		return this.#services.cwd;
	}
	get diagnostics() {
		return this.#diagnostics;
	}
	get modelFallbackMessage() {
		return this.#modelFallbackMessage;
	}
	setRebindSession(rebindSession) {
		this.#rebindSession = rebindSession;
	}
	setBeforeSessionInvalidate(beforeSessionInvalidate) {
		this.#beforeSessionInvalidate = beforeSessionInvalidate;
	}

	async #emitBeforeSwitch(reason, targetSessionFile) {
		const runner = this.session.extensionRunner;
		if (!runner.hasHandlers("session_before_switch")) return { cancelled: false };
		const result = await runner.emit({ type: "session_before_switch", reason, targetSessionFile });
		return { cancelled: result?.cancel === true };
	}

	async #emitBeforeFork(entryId, options) {
		const runner = this.session.extensionRunner;
		if (!runner.hasHandlers("session_before_fork")) return { cancelled: false };
		const result = await runner.emit({ type: "session_before_fork", entryId, ...options });
		return { cancelled: result?.cancel === true };
	}

	async #teardownCurrent(reason, targetSessionFile) {
		await this.session.abort();
		await emitSessionShutdownEvent(this.session.extensionRunner, { type: "session_shutdown", reason, targetSessionFile });
		this.#beforeSessionInvalidate?.();
		this.session.dispose();
	}

	#apply(result) {
		this.#session = result.session;
		this.#services = result.services;
		this.#diagnostics = result.diagnostics;
		this.#modelFallbackMessage = result.modelFallbackMessage;
	}

	async #finishSessionReplacement(withSession) {
		if (this.#rebindSession) await this.#rebindSession(this.session);
		if (withSession) await withSession(this.session.createReplacedSessionContext());
	}

	async switchSession(sessionPath, options) {
		const beforeResult = await this.#emitBeforeSwitch("resume", sessionPath);
		if (beforeResult.cancelled) return beforeResult;
		const previousSessionFile = this.session.sessionFile;
		const sessionManager = SessionManager.open(sessionPath, undefined, options?.cwdOverride);
		await assertSessionCwdExists(sessionManager, this.cwd);
		await this.#teardownCurrent("resume", sessionManager.getSessionFile());
		this.#apply(
			await this.#createRuntime({
				cwd: sessionManager.getCwd(),
				agentDir: this.services.agentDir,
				sessionManager,
				sessionStartEvent: { type: "session_start", reason: "resume", previousSessionFile },
				projectTrustContext: options?.projectTrustContextFactory?.(sessionManager.getCwd()),
			}),
		);
		await this.#finishSessionReplacement(options?.withSession);
		return { cancelled: false };
	}

	async newSession(options) {
		const beforeResult = await this.#emitBeforeSwitch("new");
		if (beforeResult.cancelled) return beforeResult;
		const previousSessionFile = this.session.sessionFile;
		const sessionDir = this.session.sessionManager.getSessionDir();
		const sessionManager = this.session.sessionManager.isPersisted()
			? SessionManager.create(this.cwd, sessionDir)
			: SessionManager.inMemory(this.cwd);
		if (options?.parentSession) sessionManager.newSession({ parentSession: options.parentSession });
		await this.#teardownCurrent("new", sessionManager.getSessionFile());
		this.#apply(
			await this.#createRuntime({
				cwd: this.cwd,
				agentDir: this.services.agentDir,
				sessionManager,
				sessionStartEvent: { type: "session_start", reason: "new", previousSessionFile },
			}),
		);
		if (options?.setup) {
			await options.setup(this.session.sessionManager);
			this.session.refreshContext();
		}
		await this.#finishSessionReplacement(options?.withSession);
		return { cancelled: false };
	}

	async fork(entryId, options) {
		const position = options?.position ?? "before";
		const beforeResult = await this.#emitBeforeFork(entryId, { position });
		if (beforeResult.cancelled) return { cancelled: true };
		let targetLeafId;
		let selectedText;
		const selectedEntry = this.session.sessionManager.getEntry(entryId);
		if (!selectedEntry) throw new Error("Invalid entry ID for forking");
		if (position === "at") {
			targetLeafId = selectedEntry.id;
		} else {
			if (selectedEntry.type !== "message" || selectedEntry.message.role !== "user") {
				throw new Error("Invalid entry ID for forking");
			}
			targetLeafId = selectedEntry.parentId;
			selectedText = extractUserMessageText(selectedEntry.message.content);
		}

		const previousSessionFile = this.session.sessionFile;
		const replace = async (sessionManager, cwd) => {
			this.#apply(
				await this.#createRuntime({
					cwd,
					agentDir: this.services.agentDir,
					sessionManager,
					sessionStartEvent: { type: "session_start", reason: "fork", previousSessionFile },
				}),
			);
			await this.#finishSessionReplacement(options?.withSession);
			return { cancelled: false, selectedText };
		};
		if (this.session.sessionManager.isPersisted()) {
			const currentSessionFile = this.session.sessionFile;
			if (!currentSessionFile) throw new Error("Persisted session is missing a session file");
			const sessionDir = this.session.sessionManager.getSessionDir();
			if (!targetLeafId) {
				const sessionManager = SessionManager.create(this.cwd, sessionDir);
				sessionManager.newSession({ parentSession: currentSessionFile });
				await this.#teardownCurrent("fork", sessionManager.getSessionFile());
				return replace(sessionManager, this.cwd);
			}
			const { existsSync } = await import("node:fs");
			if (!existsSync(currentSessionFile)) {
				throw new Error("This session has not been saved yet. Send a message before cloning or forking it.");
			}
			const sessionManager = SessionManager.open(currentSessionFile, sessionDir);
			const forkedSessionPath = sessionManager.createBranchedSession(targetLeafId);
			if (!forkedSessionPath) throw new Error("Failed to create forked session");
			await this.#teardownCurrent("fork", sessionManager.getSessionFile());
			return replace(sessionManager, sessionManager.getCwd());
		}

		const sessionManager = this.session.sessionManager;
		await this.#teardownCurrent("fork", sessionManager.getSessionFile());
		if (!targetLeafId) sessionManager.newSession({ parentSession: previousSessionFile });
		else sessionManager.createBranchedSession(targetLeafId);
		return replace(sessionManager, this.cwd);
	}

	async importFromJsonl(inputPath, cwdOverride) {
		const fs = await import("node:fs");
		const path = await import("node:path");
		const resolvedPath = call("sdk.servicesPaths", { a: [inputPath, null] }).cwd;
		if (!fs.existsSync(resolvedPath)) throw new SessionImportFileNotFoundError(resolvedPath);
		const sessionDir = this.session.sessionManager.getSessionDir();
		if (!fs.existsSync(sessionDir)) fs.mkdirSync(sessionDir, { recursive: true });
		let destinationPath = path.join(sessionDir, path.basename(resolvedPath));
		const sourceAlreadyStored = path.resolve(destinationPath) === resolvedPath;
		if (!sourceAlreadyStored) {
			const { name, ext } = path.parse(destinationPath);
			let suffix = 1;
			while (fs.existsSync(destinationPath)) destinationPath = path.join(sessionDir, `${name}-${suffix++}${ext}`);
		}
		const beforeResult = await this.#emitBeforeSwitch("resume", destinationPath);
		if (beforeResult.cancelled) return beforeResult;
		const previousSessionFile = this.session.sessionFile;
		if (!sourceAlreadyStored) fs.copyFileSync(resolvedPath, destinationPath, fs.constants.COPYFILE_EXCL);
		const sessionManager = SessionManager.open(destinationPath, sessionDir, cwdOverride);
		await assertSessionCwdExists(sessionManager, this.cwd);
		await this.#teardownCurrent("resume", sessionManager.getSessionFile());
		this.#apply(
			await this.#createRuntime({
				cwd: sessionManager.getCwd(),
				agentDir: this.services.agentDir,
				sessionManager,
				sessionStartEvent: { type: "session_start", reason: "resume", previousSessionFile },
			}),
		);
		await this.#finishSessionReplacement();
		return { cancelled: false };
	}

	async dispose() {
		await emitSessionShutdownEvent(this.session.extensionRunner, { type: "session_shutdown", reason: "quit" });
		this.#beforeSessionInvalidate?.();
		this.session.dispose();
	}
}

/** TS `createAgentSessionRuntime(createRuntime, options)`. */
export async function createAgentSessionRuntime(createRuntime, options) {
	await assertSessionCwdExists(options.sessionManager, options.cwd);
	const result = await createRuntime(options);
	return new AgentSessionRuntime(result.session, result.services, createRuntime, result.diagnostics, result.modelFallbackMessage);
}

// ---------------------------------------------------------------------------
// RpcClient (modes/rpc/rpc-client.ts)
// ---------------------------------------------------------------------------

function serializeJsonLine(value) {
	return `${JSON.stringify(value)}\n`;
}

/** TS `attachJsonlLineReader` (modes/rpc/jsonl.ts): strict LF-delimited JSONL lines. */
function attachJsonlLineReader(stream, decoder, onLine) {
	let buffer = "";
	const emitLine = (line) => {
		onLine(line.endsWith("\r") ? line.slice(0, -1) : line);
	};
	const onData = (chunk) => {
		buffer += typeof chunk === "string" ? chunk : decoder.write(chunk);
		while (true) {
			const newlineIndex = buffer.indexOf("\n");
			if (newlineIndex === -1) return;
			emitLine(buffer.slice(0, newlineIndex));
			buffer = buffer.slice(newlineIndex + 1);
		}
	};
	const onEnd = () => {
		buffer += decoder.end();
		if (buffer.length > 0) {
			emitLine(buffer);
			buffer = "";
		}
	};
	stream.on("data", onData);
	stream.on("end", onEnd);
	return () => {
		stream.off("data", onData);
		stream.off("end", onEnd);
	};
}

/**
 * TS `RpcClient`: drives a pi agent in `--mode rpc` over JSONL. pi-rs: without `cliPath` it spawns
 * this pi binary (`process.execPath`); a `.js`/`.ts` cliPath runs under `node` like TS, any other
 * cliPath is spawned as an executable.
 */
export class RpcClient {
	process = null;
	stopReadingStdout = null;
	eventListeners = [];
	pendingRequests = new Map();
	requestId = 0;
	stderr = "";
	exitError = null;
	options;
	constructor(options = {}) {
		this.options = options;
	}
	/**
	* Start the RPC agent process.
	*/
	async start() {
		if (this.process) {
			throw new Error("Client already started");
		}
		this.exitError = null;
		const cliPath = this.options.cliPath ?? "dist/cli.js";
		const args = ["--mode", "rpc"];
		if (this.options.provider) {
			args.push("--provider", this.options.provider);
		}
		if (this.options.model) {
			args.push("--model", this.options.model);
		}
		if (this.options.args) {
			args.push(...this.options.args);
		}
		const { spawn } = await import("node:child_process");
		const { StringDecoder } = await import("node:string_decoder");
		// pi-rs: without a cliPath the agent is this pi binary; a JS entry point runs under node as in TS.
		const [command, commandArgs] = this.options.cliPath === undefined
			? [process.execPath, args]
			: /\.[cm]?[jt]s$/.test(cliPath) ? ["node", [cliPath, ...args]] : [cliPath, args];
		const childProcess = spawn(command, commandArgs, {
			cwd: this.options.cwd,
			env: { ...process.env, ...this.options.env },
			stdio: ["pipe", "pipe", "pipe"]
		});
		this.process = childProcess;
		childProcess.stderr?.on("data", (data) => {
			this.stderr += data.toString();
			process.stderr.write(data);
		});
		childProcess.once("exit", (code, signal) => {
			if (this.process !== childProcess) return;
			const error = this.createProcessExitError(code, signal);
			this.exitError = error;
			this.rejectPendingRequests(error);
		});
		childProcess.once("error", (error) => {
			if (this.process !== childProcess) return;
			const processError = new Error(`Agent process error: ${error.message}. Stderr: ${this.stderr}`);
			this.exitError = processError;
			this.rejectPendingRequests(processError);
		});
		childProcess.stdin?.on("error", (error) => {
			if (this.process !== childProcess) return;
			const stdinError = this.exitError ?? new Error(`Agent process stdin error: ${error.message}. Stderr: ${this.stderr}`);
			this.exitError = stdinError;
			this.rejectPendingRequests(stdinError);
		});
		this.stopReadingStdout = attachJsonlLineReader(childProcess.stdout, new StringDecoder("utf8"), (line) => {
			this.handleLine(line);
		});
		await new Promise((resolve) => setTimeout(resolve, 100));
		if (this.process.exitCode !== null) {
			const error = this.exitError ?? this.createProcessExitError(this.process.exitCode, this.process.signalCode);
			this.exitError = error;
			throw error;
		}
	}
	/**
	* Stop the RPC agent process.
	*/
	async stop() {
		if (!this.process) return;
		this.stopReadingStdout?.();
		this.stopReadingStdout = null;
		this.process.kill("SIGTERM");
		await new Promise((resolve) => {
			const timeout = setTimeout(() => {
				this.process?.kill("SIGKILL");
				resolve();
			}, 1e3);
			this.process?.on("exit", () => {
				clearTimeout(timeout);
				resolve();
			});
		});
		this.process = null;
		this.pendingRequests.clear();
	}
	/**
	* Subscribe to agent events.
	*/
	onEvent(listener) {
		this.eventListeners.push(listener);
		return () => {
			const index = this.eventListeners.indexOf(listener);
			if (index !== -1) {
				this.eventListeners.splice(index, 1);
			}
		};
	}
	/**
	* Get collected stderr output (useful for debugging).
	*/
	getStderr() {
		return this.stderr;
	}
	// =========================================================================
	// Command Methods
	// =========================================================================
	/**
	* Send a prompt to the agent.
	* Returns the prompt's disposition after acceptance; use onEvent() to receive streaming events.
	* If the disposition is "handled", no run started for this prompt, so don't wait for agent_settled.
	*/
	async prompt(message, images, streamingBehavior) {
		const response = await this.send({ type: "prompt", message, images, streamingBehavior });
		return this.getData(response).disposition;
	}
	/**
	* Queue a steering message to interrupt the agent mid-run.
	*/
	async steer(message, images) {
		const response = await this.send({ type: "steer", message, images });
		return this.getData(response).disposition;
	}
	/**
	* Queue a follow-up message to be processed after the agent finishes.
	*/
	async followUp(message, images) {
		const response = await this.send({ type: "follow_up", message, images });
		return this.getData(response).disposition;
	}
	/**
	* Abort current operation.
	*/
	async abort() {
		await this.send({ type: "abort" });
	}
	/**
	* Clear queued steering and follow-up messages, returning their text.
	*/
	async clearQueue() {
		const response = await this.send({ type: "clear_queue" });
		return this.getData(response);
	}
	/**
	* Start a new session, optionally with parent tracking.
	* @param parentSession - Optional parent session path for lineage tracking
	* @returns Object with `cancelled: true` if an extension cancelled the new session
	*/
	async newSession(parentSession) {
		const response = await this.send({ type: "new_session", parentSession });
		return this.getData(response);
	}
	/**
	* Get current session state.
	*/
	async getState() {
		const response = await this.send({ type: "get_state" });
		return this.getData(response);
	}
	/**
	* Set model by provider and ID.
	*/
	async setModel(provider, modelId) {
		const response = await this.send({ type: "set_model", provider, modelId });
		return this.getData(response);
	}
	/**
	* Cycle to next model.
	*/
	async cycleModel() {
		const response = await this.send({ type: "cycle_model" });
		return this.getData(response);
	}
	/**
	* Get list of available models.
	*/
	async getAvailableModels() {
		const response = await this.send({ type: "get_available_models" });
		return this.getData(response).models;
	}
	/**
	* Set thinking level.
	*/
	async setThinkingLevel(level) {
		await this.send({ type: "set_thinking_level", level });
	}
	/**
	* Cycle thinking level.
	*/
	async cycleThinkingLevel() {
		const response = await this.send({ type: "cycle_thinking_level" });
		return this.getData(response);
	}
	/**
	* Get list of available thinking levels for the current model.
	*/
	async getAvailableThinkingLevels() {
		const response = await this.send({ type: "get_available_thinking_levels" });
		return this.getData(response).levels;
	}
	/**
	* Set steering mode.
	*/
	async setSteeringMode(mode) {
		await this.send({ type: "set_steering_mode", mode });
	}
	/**
	* Set follow-up mode.
	*/
	async setFollowUpMode(mode) {
		await this.send({ type: "set_follow_up_mode", mode });
	}
	/**
	* Compact session context.
	*/
	async compact(customInstructions) {
		const response = await this.send({ type: "compact", customInstructions });
		return this.getData(response);
	}
	/**
	* Set auto-compaction enabled/disabled.
	*/
	async setAutoCompaction(enabled) {
		await this.send({ type: "set_auto_compaction", enabled });
	}
	/**
	* Set auto-retry enabled/disabled.
	*/
	async setAutoRetry(enabled) {
		await this.send({ type: "set_auto_retry", enabled });
	}
	/**
	* Abort in-progress retry.
	*/
	async abortRetry() {
		await this.send({ type: "abort_retry" });
	}
	/**
	* Execute a bash command.
	*/
	async bash(command) {
		const response = await this.send({ type: "bash", command });
		return this.getData(response);
	}
	/**
	* Abort running bash command.
	*/
	async abortBash() {
		await this.send({ type: "abort_bash" });
	}
	/**
	* Get session statistics.
	*/
	async getSessionStats() {
		const response = await this.send({ type: "get_session_stats" });
		return this.getData(response);
	}
	/**
	* Export session to HTML.
	*/
	async exportHtml(outputPath) {
		const response = await this.send({ type: "export_html", outputPath });
		return this.getData(response);
	}
	/**
	* Switch to a different session file.
	* @returns Object with `cancelled: true` if an extension cancelled the switch
	*/
	async switchSession(sessionPath) {
		const response = await this.send({ type: "switch_session", sessionPath });
		return this.getData(response);
	}
	/**
	* Fork from a specific message.
	* @returns Object with `text` (the message text) and `cancelled` (if extension cancelled)
	*/
	async fork(entryId) {
		const response = await this.send({ type: "fork", entryId });
		return this.getData(response);
	}
	/**
	* Clone the current active branch into a new session.
	* @returns Object with `cancelled: true` if an extension cancelled the clone
	*/
	async clone() {
		const response = await this.send({ type: "clone" });
		return this.getData(response);
	}
	/**
	* Get messages available for forking.
	*/
	async getForkMessages() {
		const response = await this.send({ type: "get_fork_messages" });
		return this.getData(response).messages;
	}
	/**
	* Get session entries in append order, optionally only those after the `since` entry id.
	*/
	async getEntries(since) {
		const response = await this.send({ type: "get_entries", since });
		return this.getData(response);
	}
	/**
	* Get the session entry tree.
	*/
	async getTree() {
		const response = await this.send({ type: "get_tree" });
		return this.getData(response);
	}
	/**
	* Get text of last assistant message.
	*/
	async getLastAssistantText() {
		const response = await this.send({ type: "get_last_assistant_text" });
		return this.getData(response).text;
	}
	/**
	* Set the session display name.
	*/
	async setSessionName(name) {
		await this.send({ type: "set_session_name", name });
	}
	/**
	* Get all messages in the session.
	*/
	async getMessages() {
		const response = await this.send({ type: "get_messages" });
		return this.getData(response).messages;
	}
	/**
	* Get available commands (extension commands, prompt templates, skills).
	*/
	async getCommands() {
		const response = await this.send({ type: "get_commands" });
		return this.getData(response).commands;
	}
	// =========================================================================
	// Helpers
	// =========================================================================
	/**
	* Wait for agent to become idle (no streaming).
	* Resolves when agent_settled event is received.
	*/
	waitForIdle(timeout = 6e4) {
		return new Promise((resolve, reject) => {
			const timer = setTimeout(() => {
				unsubscribe();
				reject(new Error(`Timeout waiting for agent to become idle. Stderr: ${this.stderr}`));
			}, timeout);
			const unsubscribe = this.onEvent((event) => {
				if (event.type === "agent_settled") {
					clearTimeout(timer);
					unsubscribe();
					resolve();
				}
			});
		});
	}
	/**
	* Collect events until agent becomes idle.
	*/
	collectEvents(timeout = 6e4) {
		return new Promise((resolve, reject) => {
			const events = [];
			const timer = setTimeout(() => {
				unsubscribe();
				reject(new Error(`Timeout collecting events. Stderr: ${this.stderr}`));
			}, timeout);
			const unsubscribe = this.onEvent((event) => {
				events.push(event);
				if (event.type === "agent_settled") {
					clearTimeout(timer);
					unsubscribe();
					resolve(events);
				}
			});
		});
	}
	/**
	* Send prompt and wait for completion, returning all events.
	*/
	async promptAndWait(message, images, timeout = 6e4) {
		const eventsPromise = this.collectEvents(timeout);
		await this.prompt(message, images);
		return eventsPromise;
	}
	// =========================================================================
	// Internal
	// =========================================================================
	handleLine(line) {
		try {
			const data = JSON.parse(line);
			if (data.type === "response" && data.id && this.pendingRequests.has(data.id)) {
				const pending = this.pendingRequests.get(data.id);
				this.pendingRequests.delete(data.id);
				pending.resolve(data);
				return;
			}
			for (const listener of [...this.eventListeners]) {
				listener(data);
			}
		} catch {
		}
	}
	createProcessExitError(code, signal) {
		return new Error(`Agent process exited (code=${code} signal=${signal}). Stderr: ${this.stderr}`);
	}
	rejectPendingRequests(error) {
		for (const pending of this.pendingRequests.values()) {
			pending.reject(error);
		}
		this.pendingRequests.clear();
	}
	async send(command) {
		const childProcess = this.process;
		const stdin = childProcess?.stdin;
		if (!childProcess || !stdin) {
			throw new Error("Client not started");
		}
		if (this.exitError) {
			throw this.exitError;
		}
		if (childProcess.exitCode !== null) {
			const error = this.createProcessExitError(childProcess.exitCode, childProcess.signalCode);
			this.exitError = error;
			throw error;
		}
		if (stdin.destroyed || !stdin.writable) {
			const error = new Error(`Agent process stdin is not writable. Stderr: ${this.stderr}`);
			this.exitError = error;
			throw error;
		}
		const id = `req_${++this.requestId}`;
		const fullCommand = { ...command, id };
		return new Promise((resolve, reject) => {
			const timeout = setTimeout(() => {
				this.pendingRequests.delete(id);
				reject(new Error(`Timeout waiting for response to ${command.type}. Stderr: ${this.stderr}`));
			}, 3e4);
			this.pendingRequests.set(id, {
				resolve: (response) => {
					clearTimeout(timeout);
					resolve(response);
				},
				reject: (error) => {
					clearTimeout(timeout);
					reject(error);
				}
			});
			try {
				stdin.write(serializeJsonLine(fullCommand));
			} catch (error) {
				const writeError = error instanceof Error ? error : new Error(String(error));
				const pending = this.pendingRequests.get(id);
				this.pendingRequests.delete(id);
				pending?.reject(writeError);
			}
		});
	}
	getData(response) {
		if (!response.success) {
			const errorResponse = response;
			throw new Error(errorResponse.error);
		}
		const successResponse = response;
		return successResponse.data;
	}
}
