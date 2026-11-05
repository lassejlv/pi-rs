// `@earendil-works/pi-tui` for extensions running in pi-jsrt.
//
// Components that extensions subclass or compose (Text, Box, Container, SelectList, ...) are
// JavaScript ports of the pi-tui TypeScript components, so they can call extension callbacks
// directly. Text measurement, key parsing and Markdown rendering call the Rust pi-tui crate
// through host functions. Input and Editor are backed by the Rust components.

import { spawn } from "child_process";
import { EventEmitter } from "events";
import { readdirSync, statSync } from "fs";
import { homedir } from "os";
import { basename, dirname, join } from "path";
import {
	call,
	callAsync,
	componentRef,
	markRustBase,
	KeybindingsProxy,
	keybindingsProxy,
	RustComponent,
	rustCallback,
	tuiProxy,
	u,
} from "pi-rs:bridge";

const util = (fn, ...args) => call("tui.util", { fn, args });

// ---------------------------------------------------------------------------
// Constants and utilities
// ---------------------------------------------------------------------------

export const CURSOR_MARKER = "\x1b_pi:c\x07";
export const FAKE_CURSOR_START = "\x1b_pi:fc\x07";
export const FAKE_CURSOR_END = "\x1b_pi:/fc\x07";
export const VIEWPORT_TUI = Symbol.for("@earendil-works/pi-tui/viewport");
const LAYOUT_NODE = Symbol.for("@earendil-works/pi-tui/layout-node");

export function renderFakeCursor(text) {
	return `${FAKE_CURSOR_START}${text}${FAKE_CURSOR_END}`;
}

const widthCache = new Map();

export function visibleWidth(str) {
	if (str === undefined || str === null) return 0;
	str = String(str);
	if (str.length === 0) return 0;
	// Fast path: printable ASCII without escapes.
	let ascii = true;
	for (let i = 0; i < str.length; i++) {
		const c = str.charCodeAt(i);
		if (c < 0x20 || c > 0x7e) {
			ascii = false;
			break;
		}
	}
	if (ascii) return str.length;
	const cached = widthCache.get(str);
	if (cached !== undefined) return cached;
	const width = util("visibleWidth", str);
	if (widthCache.size > 2000) widthCache.clear();
	widthCache.set(str, width);
	return width;
}

export function truncateToWidth(text, maxWidth, ellipsis = "...", pad = false) {
	return util("truncateToWidth", String(text ?? ""), Math.floor(maxWidth), ellipsis, !!pad);
}

export function wrapTextWithAnsi(text, width) {
	return util("wrapTextWithAnsi", String(text ?? ""), Math.max(0, Math.floor(width)));
}

export function sliceByColumn(line, startCol, length, strict = false) {
	return util("sliceByColumn", String(line ?? ""), startCol, length, !!strict);
}

export function stripTerminalSequences(str) {
	return util("stripTerminalSequences", String(str ?? ""));
}

export function normalizeTerminalOutput(str) {
	return String(str ?? "");
}

export function applyBackgroundToLine(line, width, bgFn) {
	const paddingNeeded = Math.max(0, width - visibleWidth(line));
	return bgFn(line + " ".repeat(paddingNeeded));
}

export function flattenLines(_lines) {}

export const PUNCTUATION_REGEX = /[(){}[\]<>.,;:'"!?+\-=*/\\|&%^$#@~`]/;

export function isWhitespaceChar(char) {
	return /\s/.test(char);
}

export function isPunctuationChar(char) {
	return PUNCTUATION_REGEX.test(char);
}

export function matchesKey(data, keyId) {
	return !!util("matchesKey", String(data ?? ""), String(keyId ?? ""));
}

export function parseKey(data) {
	return u(util("parseKey", String(data ?? "")));
}

export function isKeyRelease(data) {
	return !!util("isKeyRelease", String(data ?? ""));
}

export function isKeyRepeat(data) {
	return !!util("isKeyRepeat", String(data ?? ""));
}

export function decodeKittyPrintable(data) {
	return u(util("decodeKittyPrintable", String(data ?? "")));
}

export function isKittyProtocolActive() {
	return !!util("isKittyProtocolActive");
}

export function setKittyProtocolActive(_active) {}

export const Key = {
	escape: "escape",
	esc: "esc",
	enter: "enter",
	return: "return",
	tab: "tab",
	space: "space",
	backspace: "backspace",
	delete: "delete",
	insert: "insert",
	clear: "clear",
	home: "home",
	end: "end",
	pageUp: "pageUp",
	pageDown: "pageDown",
	up: "up",
	down: "down",
	left: "left",
	right: "right",
	f1: "f1",
	f2: "f2",
	f3: "f3",
	f4: "f4",
	f5: "f5",
	f6: "f6",
	f7: "f7",
	f8: "f8",
	f9: "f9",
	f10: "f10",
	f11: "f11",
	f12: "f12",
	backtick: "`",
	hyphen: "-",
	equals: "=",
	leftbracket: "[",
	rightbracket: "]",
	backslash: "\\",
	semicolon: ";",
	quote: "'",
	comma: ",",
	period: ".",
	slash: "/",
	exclamation: "!",
	at: "@",
	hash: "#",
	dollar: "$",
	percent: "%",
	caret: "^",
	ampersand: "&",
	asterisk: "*",
	leftparen: "(",
	rightparen: ")",
	underscore: "_",
	plus: "+",
	pipe: "|",
	tilde: "~",
	leftbrace: "{",
	rightbrace: "}",
	colon: ":",
	lessthan: "<",
	greaterthan: ">",
	question: "?",
	ctrl: (key) => `ctrl+${key}`,
	shift: (key) => `shift+${key}`,
	alt: (key) => `alt+${key}`,
	super: (key) => `super+${key}`,
	ctrlShift: (key) => `ctrl+shift+${key}`,
	shiftCtrl: (key) => `shift+ctrl+${key}`,
	ctrlAlt: (key) => `ctrl+alt+${key}`,
	altCtrl: (key) => `alt+ctrl+${key}`,
	shiftAlt: (key) => `shift+alt+${key}`,
	altShift: (key) => `alt+shift+${key}`,
	ctrlSuper: (key) => `ctrl+super+${key}`,
	superCtrl: (key) => `super+ctrl+${key}`,
	shiftSuper: (key) => `shift+super+${key}`,
	superShift: (key) => `super+shift+${key}`,
	altSuper: (key) => `alt+super+${key}`,
	superAlt: (key) => `super+alt+${key}`,
	ctrlShiftAlt: (key) => `ctrl+shift+alt+${key}`,
	ctrlShiftSuper: (key) => `ctrl+shift+super+${key}`,
};

export function fuzzyMatch(query, text) {
	return util("fuzzyMatch", String(query ?? ""), String(text ?? ""));
}

export function fuzzyFilter(items, query, getText) {
	if (!query.trim()) return items;
	const tokens = query
		.trim()
		.split(/[\s/]+/)
		.filter((t) => t.length > 0);
	if (tokens.length === 0) return items;
	const results = [];
	for (const item of items) {
		const text = getText(item);
		let totalScore = 0;
		let allMatch = true;
		for (const token of tokens) {
			const match = fuzzyMatch(token, text);
			if (match.matches) totalScore += match.score;
			else {
				allMatch = false;
				break;
			}
		}
		if (allMatch) results.push({ item, totalScore });
	}
	results.sort((a, b) => a.totalScore - b.totalScore);
	return results.map((r) => r.item);
}

// ---------------------------------------------------------------------------
// Keybindings
// ---------------------------------------------------------------------------

/** pi-tui's built-in keybinding definitions (from `pi_tui::keybindings::TUI_KEYBINDINGS`). */
export const TUI_KEYBINDINGS = util("tuiKeybindings");

function normalizeKeys(keys) {
	if (keys === undefined) return [];
	const keyList = Array.isArray(keys) ? keys : [keys];
	const seen = new Set();
	const result = [];
	for (const key of keyList) {
		if (!seen.has(key)) {
			seen.add(key);
			result.push(key);
		}
	}
	return result;
}

// TS keybindings.ts
export class KeybindingsManager {
	keysById = new Map();
	conflicts = [];

	constructor(definitions, userBindings = {}) {
		this.definitions = definitions;
		this.userBindings = userBindings;
		this.rebuild();
	}

	rebuild() {
		if (this === globalKeybindings) syncGlobalKeybindings(this);
		this.keysById.clear();
		this.conflicts = [];
		const userClaims = new Map();
		for (const [keybinding, keys] of Object.entries(this.userBindings)) {
			if (!(keybinding in this.definitions)) continue;
			for (const key of normalizeKeys(keys)) {
				const claimants = userClaims.get(key) ?? new Set();
				claimants.add(keybinding);
				userClaims.set(key, claimants);
			}
		}
		for (const [key, keybindings] of userClaims) {
			if (keybindings.size > 1) this.conflicts.push({ key, keybindings: [...keybindings] });
		}
		for (const [id, definition] of Object.entries(this.definitions)) {
			const userKeys = this.userBindings[id];
			const keys = userKeys === undefined ? normalizeKeys(definition.defaultKeys) : normalizeKeys(userKeys);
			this.keysById.set(id, keys);
		}
	}

	matches(data, keybinding) {
		const keys = this.keysById.get(keybinding) ?? [];
		for (const key of keys) {
			if (matchesKey(data, key)) return true;
		}
		return false;
	}

	getKeys(keybinding) {
		return [...(this.keysById.get(keybinding) ?? [])];
	}

	getDefinition(keybinding) {
		return this.definitions[keybinding];
	}

	getConflicts() {
		return this.conflicts.map((conflict) => ({ ...conflict, keybindings: [...conflict.keybindings] }));
	}

	setUserBindings(userBindings) {
		this.userBindings = userBindings;
		this.rebuild();
	}

	getUserBindings() {
		return { ...this.userBindings };
	}

	getResolvedBindings() {
		const resolved = {};
		for (const id of Object.keys(this.definitions)) {
			const keys = this.keysById.get(id) ?? [];
			resolved[id] = keys.length === 1 ? keys[0] : [...keys];
		}
		return resolved;
	}
}

// The global manager defaults to pi's live keybindings (the host's, with user configuration), as
// extensions in TS share pi-tui's module state with pi. `setKeybindings()` replaces it for the
// whole app: JavaScript components read `globalKeybindings`, and the host's pi-tui global (used by
// pi's editor, selectors and Rust-backed components) is replaced by the same bindings.
let globalKeybindings = null;

function syncGlobalKeybindings(keybindings) {
	if (keybindings === null || keybindings === undefined) return;
	if (typeof keybindings.__kbId !== "undefined") {
		if (keybindings.__kbId !== null) call("kb", { m: "setGlobal", proxy: keybindings.__kbId });
		return;
	}
	if (keybindings.definitions && typeof keybindings.definitions === "object") {
		call("kb", { m: "setGlobal", definitions: keybindings.definitions, userBindings: keybindings.userBindings ?? {} });
	}
}

export function setKeybindings(keybindings) {
	globalKeybindings = keybindings;
	syncGlobalKeybindings(keybindings);
}

export function getKeybindings() {
	return globalKeybindings ?? keybindingsProxy(null);
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

export function isFocusable(component) {
	return component !== null && component !== undefined && "focused" in component;
}

export class Container {
	children = [];

	addChild(component) {
		this.children.push(component);
	}

	removeChild(component) {
		const index = this.children.indexOf(component);
		if (index !== -1) this.children.splice(index, 1);
	}

	clear() {
		this.children = [];
	}

	invalidate() {
		for (const child of this.children) child.invalidate?.();
	}

	handleMouse(event) {
		if (event.y < 0 || event.y >= event.height) return undefined;
		const mouseChildren =
			this.mouseLayout?.width === event.width
				? this.mouseLayout.children
				: this.children.map((component) => ({ component, height: component.render(event.width).length }));
		let childY = 0;
		for (const { component: child, height: childHeight } of mouseChildren) {
			if (event.y >= childY && event.y < childY + childHeight) {
				const result = dispatchMouseEvent(child, { ...event, y: event.y - childY, height: childHeight });
				if (result?.focus && this.handleInput) return { ...result, focusTarget: this };
				return result;
			}
			childY += childHeight;
		}
		return undefined;
	}

	render(width) {
		const lines = [];
		const mouseChildren = [];
		for (const child of this.children) {
			const childLines = child.render(width);
			mouseChildren.push({ component: child, height: childLines.length });
			for (const line of childLines) lines.push(line);
		}
		this.mouseLayout = { width, children: mouseChildren };
		return lines;
	}
}

/** TS `dispatchMouseEvent()` (tui.ts): dispatch to a component and keep the exact target and transform. */
function dispatchMouseEvent(component, event) {
	const result = component.handleMouse?.(event);
	if (!result) return undefined;
	if ("target" in result) {
		return result.focus && component.handleInput ? { ...result, focusTarget: component } : result;
	}
	if (!result.handled && !result.capture && !result.focus) return undefined;
	return {
		...result,
		handled: true,
		...(result.focus ? { focusTarget: component } : {}),
		target: {
			component,
			originX: event.screenX - event.x,
			originY: event.screenY - event.y,
			width: event.width,
			height: event.height,
		},
	};
}

export class Text {
	constructor(text = "", paddingX = 1, paddingY = 1, customBgFn) {
		this.text = text;
		this.paddingX = paddingX;
		this.paddingY = paddingY;
		this.customBgFn = customBgFn;
	}

	setText(text) {
		this.text = text;
		this.cachedText = undefined;
		this.cachedWidth = undefined;
		this.cachedLines = undefined;
	}

	setCustomBgFn(customBgFn) {
		this.customBgFn = customBgFn;
		this.cachedText = undefined;
		this.cachedWidth = undefined;
		this.cachedLines = undefined;
	}

	setPaddingX(paddingX) {
		this.paddingX = paddingX;
		this.invalidate();
	}

	invalidate() {
		this.cachedText = undefined;
		this.cachedWidth = undefined;
		this.cachedLines = undefined;
	}

	render(width) {
		if (this.cachedLines && this.cachedText === this.text && this.cachedWidth === width) return this.cachedLines;
		if (!this.text || String(this.text).trim() === "") {
			const result = [];
			this.cachedText = this.text;
			this.cachedWidth = width;
			this.cachedLines = result;
			return result;
		}
		const normalizedText = String(this.text).replace(/\t/g, "   ");
		const paddingX = Math.min(this.paddingX, Math.max(0, Math.floor((width - 1) / 2)));
		const contentWidth = Math.max(1, width - paddingX * 2);
		const wrappedLines = wrapTextWithAnsi(normalizedText, contentWidth);
		const leftMargin = " ".repeat(paddingX);
		const rightMargin = " ".repeat(paddingX);
		const contentLines = [];
		for (const line of wrappedLines) {
			const lineWithMargins = leftMargin + line + rightMargin;
			if (this.customBgFn) contentLines.push(applyBackgroundToLine(lineWithMargins, width, this.customBgFn));
			else {
				const paddingNeeded = Math.max(0, width - visibleWidth(lineWithMargins));
				contentLines.push(lineWithMargins + " ".repeat(paddingNeeded));
			}
		}
		const emptyLine = " ".repeat(width);
		const emptyLines = [];
		for (let i = 0; i < this.paddingY; i++) {
			emptyLines.push(this.customBgFn ? applyBackgroundToLine(emptyLine, width, this.customBgFn) : emptyLine);
		}
		const result = [...emptyLines, ...contentLines, ...emptyLines];
		this.cachedText = this.text;
		this.cachedWidth = width;
		this.cachedLines = result;
		return result.length > 0 ? result : [""];
	}
}

export class TruncatedText {
	constructor(text, paddingX = 0, paddingY = 0) {
		this.text = text;
		this.paddingX = paddingX;
		this.paddingY = paddingY;
	}

	invalidate() {}

	render(width) {
		const result = [];
		const emptyLine = " ".repeat(width);
		for (let i = 0; i < this.paddingY; i++) result.push(emptyLine);
		const availableWidth = Math.max(1, width - this.paddingX * 2);
		let singleLineText = String(this.text);
		const newlineIndex = singleLineText.indexOf("\n");
		if (newlineIndex !== -1) singleLineText = singleLineText.substring(0, newlineIndex);
		const displayText = truncateToWidth(singleLineText, availableWidth);
		const leftPadding = " ".repeat(this.paddingX);
		const lineWithPadding = leftPadding + displayText + leftPadding;
		const paddingNeeded = Math.max(0, width - visibleWidth(lineWithPadding));
		result.push(lineWithPadding + " ".repeat(paddingNeeded));
		for (let i = 0; i < this.paddingY; i++) result.push(emptyLine);
		return result;
	}
}

export class Spacer {
	constructor(lines = 1) {
		this.lines = lines;
	}

	setLines(lines) {
		this.lines = lines;
	}

	invalidate() {}

	render(_width) {
		const result = [];
		for (let i = 0; i < this.lines; i++) result.push("");
		return result;
	}
}

export class Box {
	children = [];

	constructor(paddingX = 1, paddingY = 1, bgFn) {
		this.paddingX = paddingX;
		this.paddingY = paddingY;
		this.bgFn = bgFn;
		this.cache = undefined;
	}

	addChild(component) {
		this.children.push(component);
		this.cache = undefined;
	}

	removeChild(component) {
		const index = this.children.indexOf(component);
		if (index !== -1) {
			this.children.splice(index, 1);
			this.cache = undefined;
		}
	}

	clear() {
		this.children = [];
		this.cache = undefined;
	}

	setBgFn(bgFn) {
		this.bgFn = bgFn;
	}

	setPaddingX(paddingX) {
		this.paddingX = paddingX;
		this.cache = undefined;
	}

	invalidate() {
		this.cache = undefined;
		for (const child of this.children) child.invalidate?.();
	}

	handleMouse(event) {
		const contentWidth = Math.max(1, event.width - this.paddingX * 2);
		const contentY = event.y - this.paddingY;
		const contentX = event.x - this.paddingX;
		if (contentY < 0 || contentX < 0 || contentX >= contentWidth) return undefined;
		const mouseChildren =
			this.mouseLayout?.width === contentWidth
				? this.mouseLayout.children
				: this.children.map((component) => ({ component, height: component.render(contentWidth).length }));
		let childY = 0;
		for (const { component: child, height: childHeight } of mouseChildren) {
			if (contentY >= childY && contentY < childY + childHeight) {
				return dispatchMouseEvent(child, { ...event, x: contentX, y: contentY - childY, width: contentWidth, height: childHeight });
			}
			childY += childHeight;
		}
		return undefined;
	}

	render(width) {
		if (this.children.length === 0) return [];
		const contentWidth = Math.max(1, width - this.paddingX * 2);
		const leftPad = " ".repeat(this.paddingX);
		const childLines = [];
		const mouseChildren = [];
		for (const child of this.children) {
			const lines = child.render(contentWidth);
			mouseChildren.push({ component: child, height: lines.length });
			for (const line of lines) childLines.push(line);
		}
		this.mouseLayout = { width: contentWidth, children: mouseChildren };
		if (childLines.length === 0) return [];
		const bgSample = this.bgFn ? this.bgFn("test") : undefined;
		const cache = this.cache;
		if (
			cache &&
			cache.width === width &&
			cache.bgSample === bgSample &&
			cache.childLines.length === childLines.length &&
			cache.childLines.every((line, i) => line === childLines[i])
		) {
			return cache.lines;
		}
		const result = [];
		for (let i = 0; i < this.paddingY; i++) result.push(this.applyBg("", width));
		for (const line of childLines) result.push(this.applyBg(leftPad + line, width));
		for (let i = 0; i < this.paddingY; i++) result.push(this.applyBg("", width));
		this.cache = { childLines, width, bgSample, lines: result };
		return result;
	}

	applyBg(line, width) {
		const padNeeded = Math.max(0, width - visibleWidth(line));
		const padded = line + " ".repeat(padNeeded);
		return this.bgFn ? this.bgFn(padded) : padded;
	}
}

const DEFAULT_PRIMARY_COLUMN_WIDTH = 32;
const PRIMARY_COLUMN_GAP = 2;
const MIN_DESCRIPTION_WIDTH = 10;
const normalizeToSingleLine = (text) => text.replace(/[\r\n]+/g, " ").trim();
const clamp = (value, min, max) => Math.max(min, Math.min(value, max));

export class SelectList {
	constructor(items, maxVisible, theme, layout = {}) {
		this.items = items;
		this.filteredItems = items;
		this.selectedIndex = 0;
		this.maxVisible = maxVisible;
		this.theme = theme;
		this.layout = layout;
		this.onSelect = undefined;
		this.onCancel = undefined;
		this.onSelectionChange = undefined;
	}

	setFilter(filter) {
		this.filteredItems = this.items.filter((item) => item.value.toLowerCase().startsWith(filter.toLowerCase()));
		this.selectedIndex = 0;
	}

	setSelectedIndex(index) {
		this.selectedIndex = Math.max(0, Math.min(index, this.filteredItems.length - 1));
	}

	invalidate() {}

	render(width) {
		const lines = [];
		if (this.filteredItems.length === 0) {
			lines.push(this.theme.noMatch("  No matching commands"));
			return lines;
		}
		const primaryColumnWidth = this.getPrimaryColumnWidth();
		const { startIndex, endIndex } = this.getVisibleRange();
		for (let i = startIndex; i < endIndex; i++) {
			const item = this.filteredItems[i];
			if (!item) continue;
			const isSelected = i === this.selectedIndex;
			const descriptionSingleLine = item.description ? normalizeToSingleLine(item.description) : undefined;
			lines.push(this.renderItem(item, isSelected, width, descriptionSingleLine, primaryColumnWidth));
		}
		if (startIndex > 0 || endIndex < this.filteredItems.length) {
			const scrollText = `  (${this.selectedIndex + 1}/${this.filteredItems.length})`;
			lines.push(this.theme.scrollInfo(truncateToWidth(scrollText, width - 2, "")));
		}
		return lines;
	}


	handleMouse(event) {
		if (this.filteredItems.length === 0) return undefined;
		if (event.type === "wheel" && event.wheelDelta) {
			const delta = event.wheelDelta < 0 ? -1 : 1;
			const previousIndex = this.selectedIndex;
			this.selectedIndex = Math.max(0, Math.min(this.filteredItems.length - 1, this.selectedIndex + delta));
			if (this.selectedIndex !== previousIndex) this.notifySelectionChange();
			return { handled: true, render: this.selectedIndex !== previousIndex };
		}
		// Hover must not change selection: the visible range is centered on it.
		if (event.button !== "left" || (event.type !== "press" && event.type !== "click")) return undefined;
		const { startIndex, endIndex } = this.getVisibleRange();
		const itemIndex = startIndex + event.y;
		if (itemIndex < startIndex || itemIndex >= endIndex) return undefined;
		if (event.type === "press") {
			this.mousePressedIndex = itemIndex;
			if (this.selectedIndex !== itemIndex) {
				this.selectedIndex = itemIndex;
				this.notifySelectionChange();
			}
			return { handled: true, focus: true };
		}
		if (event.type === "click") {
			const clickedIndex = this.mousePressedIndex ?? itemIndex;
			this.mousePressedIndex = undefined;
			const changed = this.selectedIndex !== clickedIndex;
			this.selectedIndex = clickedIndex;
			if (changed) this.notifySelectionChange();
			const selectedItem = this.filteredItems[this.selectedIndex];
			if (selectedItem) this.onSelect?.(selectedItem);
			return { handled: true };
		}
		return undefined;
	}
	handleInput(keyData) {
		const kb = getKeybindings();
		if (kb.matches(keyData, "tui.select.up")) {
			this.selectedIndex = this.selectedIndex === 0 ? this.filteredItems.length - 1 : this.selectedIndex - 1;
			this.notifySelectionChange();
		} else if (kb.matches(keyData, "tui.select.down")) {
			this.selectedIndex = this.selectedIndex === this.filteredItems.length - 1 ? 0 : this.selectedIndex + 1;
			this.notifySelectionChange();
		} else if (kb.matches(keyData, "tui.select.confirm")) {
			const selectedItem = this.filteredItems[this.selectedIndex];
			if (selectedItem && this.onSelect) this.onSelect(selectedItem);
		} else if (kb.matches(keyData, "tui.select.cancel")) {
			if (this.onCancel) this.onCancel();
		}
	}

	getVisibleRange() {
		const startIndex = Math.max(
			0,
			Math.min(this.selectedIndex - Math.floor(this.maxVisible / 2), this.filteredItems.length - this.maxVisible),
		);
		return { startIndex, endIndex: Math.min(startIndex + this.maxVisible, this.filteredItems.length) };
	}

	renderItem(item, isSelected, width, descriptionSingleLine, primaryColumnWidth) {
		const prefix = isSelected ? "→ " : "  ";
		const prefixWidth = visibleWidth(prefix);
		if (descriptionSingleLine && width > 40) {
			const effectivePrimaryColumnWidth = Math.max(1, Math.min(primaryColumnWidth, width - prefixWidth - 4));
			const maxPrimaryWidth = Math.max(1, effectivePrimaryColumnWidth - PRIMARY_COLUMN_GAP);
			const truncatedValue = this.truncatePrimary(item, isSelected, maxPrimaryWidth, effectivePrimaryColumnWidth);
			const truncatedValueWidth = visibleWidth(truncatedValue);
			const spacing = " ".repeat(Math.max(1, effectivePrimaryColumnWidth - truncatedValueWidth));
			const descriptionStart = prefixWidth + truncatedValueWidth + spacing.length;
			const remainingWidth = width - descriptionStart - 2;
			if (remainingWidth > MIN_DESCRIPTION_WIDTH) {
				const truncatedDesc = truncateToWidth(descriptionSingleLine, remainingWidth, "");
				if (isSelected) return this.theme.selectedText(`${prefix}${truncatedValue}${spacing}${truncatedDesc}`);
				return prefix + truncatedValue + this.theme.description(spacing + truncatedDesc);
			}
		}
		const maxWidth = width - prefixWidth - 2;
		const truncatedValue = this.truncatePrimary(item, isSelected, maxWidth, maxWidth);
		if (isSelected) return this.theme.selectedText(`${prefix}${truncatedValue}`);
		return prefix + truncatedValue;
	}

	getPrimaryColumnWidth() {
		const { min, max } = this.getPrimaryColumnBounds();
		const widestPrimary = this.filteredItems.reduce(
			(widest, item) => Math.max(widest, visibleWidth(this.getDisplayValue(item)) + PRIMARY_COLUMN_GAP),
			0,
		);
		return clamp(widestPrimary, min, max);
	}

	getPrimaryColumnBounds() {
		const rawMin =
			this.layout.minPrimaryColumnWidth ?? this.layout.maxPrimaryColumnWidth ?? DEFAULT_PRIMARY_COLUMN_WIDTH;
		const rawMax =
			this.layout.maxPrimaryColumnWidth ?? this.layout.minPrimaryColumnWidth ?? DEFAULT_PRIMARY_COLUMN_WIDTH;
		return { min: Math.max(1, Math.min(rawMin, rawMax)), max: Math.max(1, Math.max(rawMin, rawMax)) };
	}

	truncatePrimary(item, isSelected, maxWidth, columnWidth) {
		const displayValue = this.getDisplayValue(item);
		const truncatedValue = this.layout.truncatePrimary
			? this.layout.truncatePrimary({ text: displayValue, maxWidth, columnWidth, item, isSelected })
			: truncateToWidth(displayValue, maxWidth, "");
		return truncateToWidth(truncatedValue, maxWidth, "");
	}

	getDisplayValue(item) {
		return item.label || item.value;
	}

	notifySelectionChange() {
		const selectedItem = this.filteredItems[this.selectedIndex];
		if (selectedItem && this.onSelectionChange) this.onSelectionChange(selectedItem);
	}

	getSelectedItem() {
		return this.filteredItems[this.selectedIndex] || null;
	}
}

/** Single-line input backed by the Rust pi-tui `Input`. */
export class Input extends RustComponent {
	constructor(options = {}) {
		super(
			call("rc.new", {
				kind: "input",
				options: {
					prompt: options.prompt,
					placeholder: options.placeholder,
					placeholderStyle: options.placeholderStyle,
				},
			}),
		);
		this._onSubmit = undefined;
		this._onEscape = undefined;
	}
	get onSubmit() {
		return this._onSubmit;
	}
	set onSubmit(fn) {
		this._onSubmit = fn;
		call("rc", { id: this.__rid, m: "setOnSubmit", a: [fn ? (value) => this._onSubmit?.(value) : null] });
	}
	get onEscape() {
		return this._onEscape;
	}
	set onEscape(fn) {
		this._onEscape = fn;
		call("rc", { id: this.__rid, m: "setOnEscape", a: [fn ? () => this._onEscape?.() : null] });
	}
	getValue() {
		return call("rc", { id: this.__rid, m: "getValue", a: [] }) ?? "";
	}
	setValue(value) {
		call("rc", { id: this.__rid, m: "setValue", a: [String(value)] });
	}
}

/** Multi-line editor backed by the Rust pi-tui `Editor`. */
export class Editor extends RustComponent {
	constructor(tui, theme, options = {}, kind = "editor", keybindings) {
		super(
			call("rc.new", {
				kind,
				tui: tui?.__tuiId ?? null,
				keybindings: keybindings?.__kbId ?? null,
				theme: theme
					? {
							borderColor: theme.borderColor,
							selectList: theme.selectList,
						}
					: null,
				options: {
					paddingX: options?.paddingX,
					autocompleteMaxVisible: options?.autocompleteMaxVisible,
					embedWorkingStatus: options?.embedWorkingStatus,
				},
			}),
		);
		this.tui = tui;
		this.theme = theme;
		this._onSubmit = undefined;
		this._onChange = undefined;
	}
	get onSubmit() {
		return this._onSubmit;
	}
	set onSubmit(fn) {
		this._onSubmit = fn;
		call("rc", { id: this.__rid, m: "setOnSubmit", a: [fn ? (text) => this._onSubmit?.(text) : null] });
	}
	get onChange() {
		return this._onChange;
	}
	set onChange(fn) {
		this._onChange = fn;
		call("rc", { id: this.__rid, m: "setOnChange", a: [fn ? (text) => this._onChange?.(text) : null] });
	}
	get disableSubmit() {
		return !!call("rc", { id: this.__rid, m: "getDisableSubmit", a: [] });
	}
	set disableSubmit(value) {
		call("rc", { id: this.__rid, m: "setDisableSubmit", a: [!!value] });
	}
	get borderColor() {
		return this._borderColor ?? this.theme?.borderColor;
	}
	set borderColor(fn) {
		this._borderColor = fn;
		call("rc", { id: this.__rid, m: "setBorderColor", a: [fn] });
	}
	getText() {
		return call("rc", { id: this.__rid, m: "getText", a: [] }) ?? "";
	}
	getExpandedText() {
		return call("rc", { id: this.__rid, m: "getExpandedText", a: [] }) ?? "";
	}
	getLines() {
		return call("rc", { id: this.__rid, m: "getLines", a: [] }) ?? [];
	}
	getCursor() {
		return call("rc", { id: this.__rid, m: "getCursor", a: [] });
	}
	setText(text) {
		call("rc", { id: this.__rid, m: "setText", a: [String(text)] });
	}
	insertTextAtCursor(text) {
		call("rc", { id: this.__rid, m: "insertTextAtCursor", a: [String(text)] });
	}
	addToHistory(text) {
		call("rc", { id: this.__rid, m: "addToHistory", a: [String(text)] });
	}
	setPaddingX(padding) {
		call("rc", { id: this.__rid, m: "setPaddingX", a: [padding] });
	}
	getPaddingX() {
		return call("rc", { id: this.__rid, m: "getPaddingX", a: [] });
	}
	setAutocompleteMaxVisible(maxVisible) {
		call("rc", { id: this.__rid, m: "setAutocompleteMaxVisible", a: [maxVisible] });
	}
	getAutocompleteMaxVisible() {
		return call("rc", { id: this.__rid, m: "getAutocompleteMaxVisible", a: [] });
	}
	setAutocompleteProvider(provider) {
		call("rc", { id: this.__rid, m: "setAutocompleteProvider", a: [provider] });
	}
	isShowingAutocomplete() {
		return !!call("rc", { id: this.__rid, m: "isShowingAutocomplete", a: [] });
	}
}

markRustBase(Input);
markRustBase(Editor);

/** Markdown component: rendering is done by the Rust pi-tui Markdown renderer. */
export class Markdown {
	constructor(text, paddingX, paddingY, theme, defaultTextStyle, options) {
		this.text = text;
		this.paddingX = paddingX;
		this.paddingY = paddingY;
		this.theme = theme;
		this.defaultTextStyle = defaultTextStyle;
		this.options = options ? { ...options } : {};
	}

	setText(text) {
		this.text = text;
		this.invalidate();
	}

	invalidate() {
		this.cachedText = undefined;
		this.cachedWidth = undefined;
		this.cachedLines = undefined;
	}

	render(width) {
		if (this.cachedLines && this.cachedText === this.text && this.cachedWidth === width) return this.cachedLines;
		const lines = call("tui.markdown", {
			text: String(this.text ?? ""),
			paddingX: this.paddingX ?? 0,
			paddingY: this.paddingY ?? 0,
			theme: markdownThemeRef(this.theme),
			defaultTextStyle: this.defaultTextStyle ?? null,
			options: this.options,
			width,
		});
		this.cachedText = this.text;
		this.cachedWidth = width;
		this.cachedLines = lines;
		return lines;
	}
}

const markdownThemes = new WeakMap();

function markdownThemeRef(theme) {
	if (!theme) return null;
	if (theme.__mdTheme !== undefined) return { builtin: theme.__mdTheme };
	let id = markdownThemes.get(theme);
	if (id === undefined) {
		id = call("tui.markdownTheme", { theme });
		markdownThemes.set(theme, id);
	}
	return { id };
}

const DEFAULT_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const DEFAULT_INTERVAL_MS = 80;

export class Loader extends Text {
	constructor(ui, spinnerColorFn, messageColorFn, message = "Loading...", indicator) {
		super("", 1, 0);
		this.frames = [...DEFAULT_FRAMES];
		this.intervalMs = DEFAULT_INTERVAL_MS;
		this.currentFrame = 0;
		this.intervalId = null;
		this.ui = ui;
		this.renderIndicatorVerbatim = false;
		this.spinnerColorFn = spinnerColorFn;
		this.messageColorFn = messageColorFn;
		this.message = message;
		this.setIndicator(indicator);
	}

	render(width) {
		return ["", ...super.render(width)];
	}

	start() {
		this.updateDisplay();
		this.restartAnimation();
	}

	stop() {
		if (this.intervalId) {
			clearInterval(this.intervalId);
			this.intervalId = null;
		}
	}

	setMessage(message) {
		this.message = message;
		this.updateDisplay();
	}

	invalidate() {
		super.invalidate();
		if (this.messageColorFn) this.updateDisplay();
	}

	setIndicator(indicator) {
		this.renderIndicatorVerbatim = indicator !== undefined;
		this.frames = indicator?.frames !== undefined ? [...indicator.frames] : [...DEFAULT_FRAMES];
		this.intervalMs = indicator?.intervalMs && indicator.intervalMs > 0 ? indicator.intervalMs : DEFAULT_INTERVAL_MS;
		this.currentFrame = 0;
		this.start();
	}

	restartAnimation() {
		this.stop();
		if (this.frames.length <= 1) return;
		this.intervalId = setInterval(() => {
			this.currentFrame = (this.currentFrame + 1) % this.frames.length;
			this.updateDisplay();
		}, this.intervalMs);
		this.intervalId?.unref?.();
	}

	getRenderedIndicator() {
		const frame = this.frames[this.currentFrame] ?? "";
		return this.renderIndicatorVerbatim ? frame : this.spinnerColorFn(frame);
	}

	updateDisplay() {
		const renderedFrame = this.getRenderedIndicator();
		const indicator = renderedFrame.length > 0 ? `${renderedFrame} ` : "";
		this.setText(`${indicator}${this.messageColorFn(this.message)}`);
		if (this.ui) this.ui.requestRender();
	}
}

export class CancellableLoader extends Loader {
	constructor(...args) {
		super(...args);
		this.abortController = new AbortController();
		this.onAbort = undefined;
	}

	get signal() {
		return this.abortController.signal;
	}

	get aborted() {
		return this.abortController.signal.aborted;
	}

	handleInput(data) {
		if (getKeybindings().matches(data, "tui.select.cancel")) {
			this.abortController.abort();
			this.onAbort?.();
		}
	}

	dispose() {
		this.stop();
	}
}

export class SettingsList {
	constructor(items, maxVisible, theme, onChange, onCancel, options = {}) {
		this.items = items;
		this.filteredItems = items;
		this.maxVisible = maxVisible;
		this.theme = theme;
		this.onChange = onChange;
		this.onCancel = onCancel;
		this.selectedIndex = 0;
		this.searchEnabled = options.enableSearch ?? false;
		if (this.searchEnabled) this.searchInput = new Input();
		this.submenuComponent = null;
		this.submenuItemIndex = null;
		this.navigateAfterClose = null;
	}

	updateValue(id, newValue) {
		const item = this.items.find((i) => i.id === id);
		if (item) item.currentValue = newValue;
	}

	selectItem(id) {
		const items = this.searchEnabled ? this.filteredItems : this.items;
		const index = items.findIndex((i) => i.id === id);
		if (index !== -1) this.selectedIndex = index;
	}

	invalidate() {
		this.submenuComponent?.invalidate?.();
	}

	render(width) {
		if (this.submenuComponent) return this.submenuComponent.render(width);
		return this.renderMainList(width);
	}

	renderMainList(width) {
		const lines = [];
		if (this.searchEnabled && this.searchInput) {
			lines.push(...this.searchInput.render(width));
			lines.push("");
		}
		if (this.items.length === 0) {
			lines.push(this.theme.hint("  No settings available"));
			if (this.searchEnabled) this.addHintLine(lines, width);
			return lines;
		}
		const displayItems = this.getDisplayItems();
		if (displayItems.length === 0) {
			lines.push(truncateToWidth(this.theme.hint("  No matching settings"), width));
			this.addHintLine(lines, width);
			return lines;
		}
		const { startIndex, endIndex } = this.getVisibleRange(displayItems);
		const maxLabelWidth = Math.min(36, Math.max(...this.items.map((item) => visibleWidth(item.label))));
		for (let i = startIndex; i < endIndex; i++) {
			const item = displayItems[i];
			if (!item) continue;
			const isSelected = i === this.selectedIndex;
			const prefix = isSelected ? this.theme.cursor : "  ";
			const prefixWidth = visibleWidth(prefix);
			const labelPadded = item.label + " ".repeat(Math.max(0, maxLabelWidth - visibleWidth(item.label)));
			const labelText = this.theme.label(labelPadded, isSelected);
			const separator = "  ";
			const usedWidth = prefixWidth + maxLabelWidth + visibleWidth(separator);
			const valueMaxWidth = width - usedWidth - 2;
			const valueText = this.theme.value(truncateToWidth(item.currentValue, valueMaxWidth, ""), isSelected);
			lines.push(truncateToWidth(prefix + labelText + separator + valueText, width));
		}
		if (startIndex > 0 || endIndex < displayItems.length) {
			const scrollText = `  (${this.selectedIndex + 1}/${displayItems.length})`;
			lines.push(this.theme.hint(truncateToWidth(scrollText, width - 2, "")));
		}
		const selectedItem = displayItems[this.selectedIndex];
		if (selectedItem?.description) {
			lines.push("");
			for (const line of wrapTextWithAnsi(selectedItem.description, width - 4)) {
				lines.push(this.theme.description(`  ${line}`));
			}
		}
		this.addHintLine(lines, width);
		return lines;
	}


	handleMouse(event) {
		if (this.submenuComponent) {
			const result = this.submenuComponent.handleMouse?.(event);
			return result ? { ...result, focus: true } : undefined;
		}
		if (this.searchEnabled && this.searchInput) {
			if (event.y === 0) {
				const result = this.searchInput.handleMouse?.(event);
				return result ? { ...result, focus: true } : undefined;
			}
			if (event.y === 1) return undefined;
		}
		const displayItems = this.getDisplayItems();
		if (displayItems.length === 0) return undefined;
		if (event.type === "wheel" && event.wheelDelta) {
			const delta = event.wheelDelta < 0 ? -1 : 1;
			const previousIndex = this.selectedIndex;
			this.selectedIndex = Math.max(0, Math.min(displayItems.length - 1, this.selectedIndex + delta));
			return { handled: true, render: this.selectedIndex !== previousIndex };
		}
		// Hover must not change selection: the visible range is centered on it.
		if (event.button !== "left" || (event.type !== "press" && event.type !== "click")) return undefined;
		const rowOffset = this.searchEnabled ? 2 : 0;
		const { startIndex, endIndex } = this.getVisibleRange(displayItems);
		const itemIndex = startIndex + event.y - rowOffset;
		if (itemIndex < startIndex || itemIndex >= endIndex) return undefined;
		if (event.type === "press") {
			this.mousePressedIndex = itemIndex;
			this.selectedIndex = itemIndex;
			return { handled: true, focus: true };
		}
		if (event.type === "click") {
			this.selectedIndex = this.mousePressedIndex ?? itemIndex;
			this.mousePressedIndex = undefined;
			this.activateItem();
			return { handled: true };
		}
		return undefined;
	}
	handleInput(data) {
		if (this.submenuComponent) {
			this.submenuComponent.handleInput?.(data);
			return;
		}
		const kb = getKeybindings();
		const displayItems = this.getDisplayItems();
		if (kb.matches(data, "tui.select.up")) {
			if (displayItems.length === 0) return;
			this.selectedIndex = this.selectedIndex === 0 ? displayItems.length - 1 : this.selectedIndex - 1;
		} else if (kb.matches(data, "tui.select.down")) {
			if (displayItems.length === 0) return;
			this.selectedIndex = this.selectedIndex === displayItems.length - 1 ? 0 : this.selectedIndex + 1;
		} else if (
			kb.matches(data, "tui.select.confirm") ||
			(data === " " && (!this.searchEnabled || this.searchInput?.getValue().length === 0))
		) {
			this.activateItem();
		} else if (kb.matches(data, "tui.select.cancel")) {
			this.onCancel();
		} else if (this.searchEnabled && this.searchInput) {
			this.searchInput.handleInput(data);
			this.applyFilter(this.searchInput.getValue());
		}
	}

	getDisplayItems() {
		return this.searchEnabled ? this.filteredItems : this.items;
	}

	getVisibleRange(displayItems) {
		const startIndex = Math.max(
			0,
			Math.min(this.selectedIndex - Math.floor(this.maxVisible / 2), displayItems.length - this.maxVisible),
		);
		return { startIndex, endIndex: Math.min(startIndex + this.maxVisible, displayItems.length) };
	}

	activateItem() {
		const item = this.getDisplayItems()[this.selectedIndex];
		if (!item) return;
		if (item.submenu) {
			this.submenuItemIndex = this.selectedIndex;
			this.submenuComponent = item.submenu(item.currentValue, (selectedValue, options) => {
				if (selectedValue !== undefined) {
					item.currentValue = selectedValue;
					this.onChange(item.id, selectedValue);
				}
				if (options?.navigateTo) this.navigateAfterClose = options.navigateTo;
				this.closeSubmenu();
			});
		} else if (item.values && item.values.length > 0) {
			const currentIndex = item.values.indexOf(item.currentValue);
			const nextIndex = (currentIndex + 1) % item.values.length;
			const newValue = item.values[nextIndex];
			item.currentValue = newValue;
			this.onChange(item.id, newValue);
		}
	}

	closeSubmenu() {
		this.submenuComponent = null;
		if (this.navigateAfterClose !== null) {
			const id = this.navigateAfterClose;
			this.navigateAfterClose = null;
			this.submenuItemIndex = null;
			this.selectItem(id);
			this.activateItem();
		} else if (this.submenuItemIndex !== null) {
			this.selectedIndex = this.submenuItemIndex;
			this.submenuItemIndex = null;
		}
	}

	applyFilter(query) {
		this.filteredItems = fuzzyFilter(this.items, query, (item) => item.label);
		this.selectedIndex = 0;
	}

	addHintLine(lines, width) {
		lines.push("");
		lines.push(
			truncateToWidth(
				this.theme.hint(
					this.searchEnabled
						? "  Type to search · Enter/Space to change · Esc to cancel"
						: "  Enter/Space to change · Esc to cancel",
				),
				width,
			),
		);
	}
}

// TS components/image.ts
let imageTranscoder;
// Backstop for callers that recreate Image instances. Keyed by source data, least recently used first.
const pngCache = new Map();

export function setImageTranscoder(transcoder) {
	imageTranscoder = transcoder;
	pngCache.clear();
}

function toPng(base64Data, mimeType) {
	if (!imageTranscoder) {
		// pi's own transcoder (registered by the host on Kitty terminals), shared like pi-tui's module state.
		const png = util("hostTranscodeToPng", base64Data, mimeType);
		return png === false ? null : (png ?? null);
	}
	const cached = pngCache.get(base64Data);
	const png = cached === undefined ? imageTranscoder(base64Data, mimeType) : cached;
	pngCache.delete(base64Data);
	pngCache.set(base64Data, png);
	if (pngCache.size > 32) pngCache.delete(pngCache.keys().next().value);
	return png;
}

export class Image {
	constructor(base64Data, mimeType, theme, options = {}, dimensions) {
		this.base64Data = base64Data;
		this.mimeType = mimeType;
		this.theme = theme;
		this.options = options;
		this.dimensions = dimensions || getImageDimensions(base64Data, mimeType) || { widthPx: 800, heightPx: 600 };
		this.imageId = options.imageId;
	}

	getImageId() {
		return this.imageId;
	}

	invalidate() {
		this.cachedLines = undefined;
		this.cachedWidth = undefined;
	}

	render(width) {
		if (this.cachedLines && this.cachedWidth === width) return this.cachedLines;
		const maxWidth = Math.max(1, Math.min(width - 2, this.options.maxWidthCells ?? 60));
		const cellDimensions = getCellDimensions();
		const defaultMaxHeight = Math.max(1, Math.ceil((maxWidth * cellDimensions.widthPx) / cellDimensions.heightPx));
		const maxHeight = this.options.maxHeightCells ?? defaultMaxHeight;
		const caps = getCapabilities();
		let data = this.base64Data;
		let dimensions = this.dimensions;
		if (caps.images === "kitty" && this.mimeType !== "image/png") {
			this.pngData ??= toPng(this.base64Data, this.mimeType) ?? undefined;
			data = this.pngData ?? null;
			// Conversion may apply EXIF rotation, so prefer the PNG's own dimensions.
			if (data) dimensions = getPngDimensions(data) ?? dimensions;
		}
		let lines;
		const result =
			caps.images && data
				? (() => {
						if (caps.images === "kitty" && this.imageId === undefined) this.imageId = allocateImageId();
						return renderImage(data, dimensions, {
							maxWidthCells: maxWidth,
							maxHeightCells: maxHeight,
							imageId: this.imageId,
							moveCursor: false,
						});
					})()
				: undefined;
		if (result) {
			if (result.imageId) this.imageId = result.imageId;
			if (caps.images === "kitty") {
				lines = [result.sequence];
				for (let i = 0; i < result.rows - 1; i++) lines.push("");
			} else {
				lines = [];
				for (let i = 0; i < result.rows - 1; i++) lines.push("");
				const rowOffset = result.rows - 1;
				const moveUp = rowOffset > 0 ? `\x1b[${rowOffset}A` : "";
				lines.push(moveUp + result.sequence);
			}
		} else {
			const fallback = imageFallback(this.mimeType, this.dimensions, this.options.filename);
			lines = [truncateToWidth(this.theme.fallbackColor(fallback), width)];
		}
		this.cachedLines = lines;
		this.cachedWidth = width;
		return lines;
	}
}

// Stacks, scroll views and mouse regions (TS components/stack.ts, h-stack.ts, v-stack.ts,
// scroll-view.ts, mouse-region.ts). Like TS, pi's fullscreen (viewport) TUI lays out components
// with `[LAYOUT_NODE]()` itself (bridge.js `describeLayoutNode`); elsewhere they use `render()`.

function isStackEntry(child) {
  return !("render" in child);
}
function normalizeSize(value, fallback) {
  return value === void 0 || !Number.isFinite(value) ? fallback : Math.max(0, Math.floor(value));
}
export class Stack extends Container {
  entries = [];
  gap;
  align;
  constructor(children = [], options = {}) {
    super();
    this.gap = normalizeSize(options.gap, 0);
    this.align = options.align ?? "stretch";
    for (const child of children) {
      if (isStackEntry(child)) this.addChild(child.component, child);
      else this.addChild(child);
    }
  }
  addChild(component, options = {}) {
    super.addChild(component);
    this.entries.push({
      component,
      ...options.basis === void 0 ? {} : { basis: options.basis },
      ...options.grow === void 0 ? {} : { grow: normalizeSize(options.grow, 0) },
      ...options.shrink === void 0 ? {} : { shrink: normalizeSize(options.shrink, 1) },
      ...options.minSize === void 0 ? {} : { minSize: normalizeSize(options.minSize, 0) },
      ...options.maxSize === void 0 ? {} : { maxSize: normalizeSize(options.maxSize, Number.MAX_SAFE_INTEGER) },
      ...options.visible === void 0 ? {} : { visible: options.visible }
    });
  }
  removeChild(component) {
    super.removeChild(component);
    const index = this.entries.findIndex((entry) => entry.component === component);
    if (index !== -1) this.entries.splice(index, 1);
  }
  clear() {
    super.clear();
    this.entries.length = 0;
  }
  [LAYOUT_NODE]() {
    return {
      type: this.layoutType,
      entries: this.entries,
      gap: this.gap,
      align: this.align
    };
  }
}
export function visibleStackEntries(entries, viewport) {
  return entries.filter((entry) => entry.visible?.(viewport) ?? true);
}
function clampSize(size, entry) {
  const min = Math.max(0, Math.floor(entry.minSize ?? 0));
  const max = Math.max(min, Math.floor(entry.maxSize ?? Number.MAX_SAFE_INTEGER));
  return Math.max(min, Math.min(max, Math.max(0, Math.floor(size))));
}
function distribute(sizes, entries, amount, mode) {
  let remaining = amount;
  while (remaining > 0) {
    const candidates = entries.map((entry, index) => ({ entry, index })).filter(({ entry, index }) => {
      if (mode === "grow") {
        return (entry.grow ?? 0) > 0 && sizes[index] < (entry.maxSize ?? Number.MAX_SAFE_INTEGER);
      }
      return (entry.shrink ?? 1) > 0 && sizes[index] > (entry.minSize ?? 0);
    });
    if (candidates.length === 0) return;
    const totalWeight = candidates.reduce((sum, { entry, index }) => {
      return sum + (mode === "grow" ? entry.grow ?? 0 : (entry.shrink ?? 1) * Math.max(1, sizes[index]));
    }, 0);
    let distributed = 0;
    for (const { entry, index } of candidates) {
      if (remaining <= 0) break;
      const weight = mode === "grow" ? entry.grow ?? 0 : (entry.shrink ?? 1) * Math.max(1, sizes[index]);
      const proposed = Math.max(1, Math.floor(remaining * weight / totalWeight));
      const capacity = mode === "grow" ? (entry.maxSize ?? Number.MAX_SAFE_INTEGER) - sizes[index] : sizes[index] - (entry.minSize ?? 0);
      const delta = Math.min(remaining, proposed, capacity);
      if (delta <= 0) continue;
      sizes[index] = sizes[index] + (mode === "grow" ? delta : -delta);
      remaining -= delta;
      distributed += delta;
    }
    if (distributed === 0) return;
  }
}
export function allocateStackSizes(entries, intrinsicSizes, availableSize, gap) {
  const sizes = entries.map(
    (entry, index) => clampSize(
      entry.basis === void 0 || entry.basis === "auto" ? intrinsicSizes[index] ?? 0 : entry.basis,
      entry
    )
  );
  if (availableSize === void 0) return sizes;
  const contentSize = Math.max(0, Math.floor(availableSize) - Math.max(0, entries.length - 1) * gap);
  const total = sizes.reduce((sum, size) => sum + size, 0);
  if (total < contentSize) distribute(sizes, entries, contentSize - total, "grow");
  else if (total > contentSize) distribute(sizes, entries, total - contentSize, "shrink");
  return sizes;
}

export class HStack extends Stack {
  layoutType = "hstack";
  constructor(children = [], options = {}) {
    super(children, options);
  }
  render(width) {
    const safeWidth = Math.max(1, width);
    const viewport = { width: safeWidth, height: Number.MAX_SAFE_INTEGER };
    const entries = visibleStackEntries(this.entries, viewport);
    if (entries.length === 0) return [];
    const intrinsicWidths = entries.map((entry) => {
      const lines = entry.component.render(safeWidth);
      return lines.reduce((max, line) => Math.max(max, visibleWidth(line)), 0);
    });
    const widths = allocateStackSizes(entries, intrinsicWidths, safeWidth, this.gap);
    const rendered = entries.map(
      (entry, index) => widths[index] === 0 ? [] : entry.component.render(widths[index])
    );
    const height = rendered.reduce((max, lines) => Math.max(max, lines.length), 0);
    const result = Array.from({ length: height }, () => "");
    let x = 0;
    for (let index = 0; index < rendered.length; index++) {
      const lines = rendered[index];
      const childWidth = widths[index];
      let offset = 0;
      if (this.align === "center") offset = Math.floor((height - lines.length) / 2);
      else if (this.align === "end") offset = height - lines.length;
      for (let row = 0; row < lines.length; row++) {
        const target = row + offset;
        if (target < 0 || target >= result.length) continue;
        result[target] = compositeTuiLine(result[target], lines[row], x, childWidth, safeWidth);
      }
      x += childWidth + this.gap;
    }
    return result;
  }
}

export class VStack extends Stack {
  layoutType = "vstack";
  constructor(children = [], options = {}) {
    super(children, options);
  }
  render(width) {
    const viewport = { width: Math.max(1, width), height: Number.MAX_SAFE_INTEGER };
    const entries = visibleStackEntries(this.entries, viewport);
    const rendered = entries.map((entry) => entry.component.render(viewport.width));
    const sizes = allocateStackSizes(
      entries,
      rendered.map((lines2) => lines2.length),
      void 0,
      this.gap
    );
    const lines = [];
    for (let index = 0; index < entries.length; index++) {
      if (index > 0) {
        for (let gap = 0; gap < this.gap; gap++) lines.push("");
      }
      const childLines = rendered[index].slice(0, sizes[index]);
      lines.push(...childLines);
      for (let padding = childLines.length; padding < sizes[index]; padding++) lines.push("");
    }
    return lines;
  }
}

export class ScrollView extends Container {
  child;
  followEnd;
  primary;
  overscroll;
  scrollbarTrackStyle;
  scrollbarThumbStyle;
  currentScrollbar;
  scrollbarHideDelayMs;
  currentScrollTop = 0;
  contentHeight = 0;
  currentViewportHeight = 0;
  followingEnd;
  followSuppressedAtEnd = false;
  requestRenderCallback;
  transientScrollbarVisible = false;
  scrollbarActive = false;
  scrollbarHideTimer;
  constructor(component, options = {}) {
    super();
    if (options.axis !== void 0 && options.axis !== "vertical") {
      throw new Error(`Unsupported ScrollView axis: ${options.axis}`);
    }
    this.child = component;
    this.children.push(component);
    this.followEnd = (options.follow ?? "none") === "end";
    this.followingEnd = this.followEnd;
    this.primary = options.primary ?? false;
    this.overscroll = options.overscroll ?? "chain";
    this.currentScrollbar = options.scrollbar ?? "hidden";
    this.scrollbarTrackStyle = options.scrollbarTrackStyle ?? ((text) => `\x1B[90m${text}\x1B[39m`);
    this.scrollbarThumbStyle = options.scrollbarThumbStyle ?? ((text) => `\x1B[37m${text}\x1B[39m`);
    this.scrollbarHideDelayMs = Math.max(0, Math.floor(options.scrollbarHideDelayMs ?? 1e3));
  }
  // pi-rs: while pi's fullscreen layout lays this view out, a Rust ScrollView mirror owns the
  // scroll state (wheel scrolling, follow-end, scrollbar); reads and writes go through it.
  __svAttach(id) {
    Object.defineProperty(this, "__svMirror", { value: id, writable: true, configurable: true, enumerable: false });
  }
  __svInit() {
    return {
      follow: this.followEnd ? "end" : "none",
      primary: this.primary,
      overscroll: this.overscroll,
      scrollbar: this.currentScrollbar,
      trackStyle: this.scrollbarTrackStyle,
      thumbStyle: this.scrollbarThumbStyle,
      hideDelayMs: this.scrollbarHideDelayMs,
      scrollTop: this.currentScrollTop,
      contentHeight: this.contentHeight,
      viewportHeight: this.currentViewportHeight,
      followingEnd: this.followingEnd,
      scrollbarActive: this.scrollbarActive
    };
  }
  __svCall(m, a = []) {
    if (this.__svMirror === undefined) return undefined;
    const response = call("sv", { id: this.__svMirror, m, a });
    if (response === null || response === undefined) {
      this.__svMirror = undefined;
      return undefined;
    }
    const state = response.state;
    this.currentScrollTop = state.scrollTop;
    this.followingEnd = state.followingEnd;
    this.currentViewportHeight = state.viewportHeight;
    this.contentHeight = state.contentHeight;
    this.currentScrollbar = state.scrollbar;
    this.scrollbarActive = state.scrollbarActive;
    this.transientScrollbarVisible = state.scrollbarVisible;
    return response;
  }
  get scrollTop() {
    this.__svCall("state");
    return this.currentScrollTop;
  }
  get isFollowingEnd() {
    this.__svCall("state");
    return this.followingEnd;
  }
  get viewportHeight() {
    this.__svCall("state");
    return this.currentViewportHeight;
  }
  get scrollbar() {
    this.__svCall("state");
    return this.currentScrollbar;
  }
  get isScrollbarVisible() {
    if (this.__svCall("state")) {
      if (this.currentScrollbar === "always") return this.currentViewportHeight > 0;
      return this.currentScrollbar === "auto" && this.transientScrollbarVisible;
    }
    if (this.scrollbar === "always") return this.currentViewportHeight > 0;
    return this.scrollbar === "auto" && this.contentHeight > this.currentViewportHeight && this.transientScrollbarVisible;
  }
  get isScrollbarActive() {
    this.__svCall("state");
    return this.scrollbarActive;
  }
  setScrollbar(scrollbar) {
    if (this.__svCall("setScrollbar", [scrollbar])) return;
    if (scrollbar === this.currentScrollbar) return;
    this.currentScrollbar = scrollbar;
    if (scrollbar !== "auto") this.hideTransientScrollbar();
    else if (this.scrollbarActive) this.markScrollbarActivity();
    this.requestRenderCallback?.();
  }
  getContentWidth(width) {
    return this.scrollbar === "always" && width > 1 ? width - 1 : width;
  }
  markScrollbarActivity() {
    if (this.scrollbar !== "auto" || this.contentHeight <= this.currentViewportHeight) return;
    this.transientScrollbarVisible = true;
    if (this.scrollbarHideTimer) {
      clearTimeout(this.scrollbarHideTimer);
      this.scrollbarHideTimer = void 0;
    }
    if (this.scrollbarActive) return;
    this.scrollbarHideTimer = setTimeout(() => {
      this.scrollbarHideTimer = void 0;
      this.transientScrollbarVisible = false;
      this.requestRenderCallback?.();
    }, this.scrollbarHideDelayMs);
    this.scrollbarHideTimer?.unref?.();
  }
  hideTransientScrollbar() {
    this.transientScrollbarVisible = false;
    if (!this.scrollbarHideTimer) return;
    clearTimeout(this.scrollbarHideTimer);
    this.scrollbarHideTimer = void 0;
  }
  setScrollbarActive(active) {
    if (this.__svCall("setScrollbarActive", [!!active])) return;
    if (active === this.scrollbarActive) return;
    this.scrollbarActive = active;
    this.markScrollbarActivity();
    this.requestRenderCallback?.();
  }
  scrollTo(scrollTop, options = {}) {
    if (this.__svCall("scrollTo", [scrollTop, { disableFollow: options.disableFollow === true }])) return;
    const requested = Number.isFinite(scrollTop) ? Math.trunc(scrollTop) : this.currentScrollTop;
    const maxScrollTop = Math.max(0, this.contentHeight - this.currentViewportHeight);
    const next = Math.max(0, Math.min(maxScrollTop, requested));
    const nextFollowSuppressedAtEnd = options.disableFollow === true && next === maxScrollTop;
    const nextFollowingEnd = !nextFollowSuppressedAtEnd && this.followEnd && next === maxScrollTop;
    if (next === this.currentScrollTop && nextFollowingEnd === this.followingEnd && nextFollowSuppressedAtEnd === this.followSuppressedAtEnd) {
      return;
    }
    const moved = next !== this.currentScrollTop;
    this.currentScrollTop = next;
    this.followingEnd = nextFollowingEnd;
    this.followSuppressedAtEnd = nextFollowSuppressedAtEnd;
    if (moved) this.markScrollbarActivity();
    this.requestRenderCallback?.();
  }
  scrollBy(lines) {
    const mirrored = this.__svCall("scrollBy", [lines]);
    if (mirrored) return mirrored.result;
    const requested = Number.isFinite(lines) ? Math.trunc(lines) : 0;
    if (requested === 0) return 0;
    const maxScrollTop = Math.max(0, this.contentHeight - this.currentViewportHeight);
    const start = this.followingEnd ? maxScrollTop : this.currentScrollTop;
    const next = Math.max(0, Math.min(maxScrollTop, start + requested));
    const moved = next - start;
    const wasFollowingEnd = this.followingEnd;
    this.currentScrollTop = next;
    this.followingEnd = this.followEnd && next === maxScrollTop;
    this.followSuppressedAtEnd = false;
    if (moved !== 0) this.markScrollbarActivity();
    if (moved !== 0 || this.followingEnd !== wasFollowingEnd) this.requestRenderCallback?.();
    return requested - moved;
  }
  scrollToStart() {
    if (this.__svCall("scrollToStart")) return;
    const changed = this.currentScrollTop !== 0 || this.followingEnd !== (this.followEnd && this.contentHeight <= this.currentViewportHeight);
    this.currentScrollTop = 0;
    this.followingEnd = this.followEnd && this.contentHeight <= this.currentViewportHeight;
    this.followSuppressedAtEnd = false;
    if (changed) {
      this.markScrollbarActivity();
      this.requestRenderCallback?.();
    }
  }
  scrollToEnd() {
    if (this.__svCall("scrollToEnd")) return;
    const next = Math.max(0, this.contentHeight - this.currentViewportHeight);
    const changed = this.currentScrollTop !== next || this.followingEnd !== this.followEnd;
    this.currentScrollTop = next;
    this.followingEnd = this.followEnd;
    this.followSuppressedAtEnd = false;
    if (changed) {
      this.markScrollbarActivity();
      this.requestRenderCallback?.();
    }
  }
  updateLayout(contentHeight, viewportHeight, requestRender) {
    if (this.__svCall("updateLayout", [contentHeight, viewportHeight])) {
      this.requestRenderCallback = requestRender;
      return;
    }
    this.contentHeight = Math.max(0, Math.floor(contentHeight));
    this.currentViewportHeight = Math.max(0, Math.floor(viewportHeight));
    this.requestRenderCallback = requestRender;
    const maxScrollTop = Math.max(0, this.contentHeight - this.currentViewportHeight);
    if (this.followingEnd) this.currentScrollTop = maxScrollTop;
    else this.currentScrollTop = Math.max(0, Math.min(this.currentScrollTop, maxScrollTop));
    if (this.currentScrollTop < maxScrollTop) this.followSuppressedAtEnd = false;
    if (this.followEnd && this.currentScrollTop === maxScrollTop && !this.followSuppressedAtEnd) {
      this.followingEnd = true;
    }
    if (this.contentHeight <= this.currentViewportHeight) this.hideTransientScrollbar();
  }
  addChild(_component) {
    throw new Error("ScrollView has exactly one child");
  }
  removeChild(_component) {
    throw new Error("ScrollView child cannot be removed");
  }
  clear() {
    throw new Error("ScrollView child cannot be cleared");
  }
  render(width) {
    const contentWidth = this.getContentWidth(width);
    const lines = this.child.render(contentWidth);
    return contentWidth === width ? lines : lines.map((line) => `${line} `);
  }
  [LAYOUT_NODE]() {
    return { type: "scroll", component: this.child, state: this };
  }
}

export class MouseRegion {
  child;
  onMouse;
  constructor(child, onMouse) {
    this.child = child;
    this.onMouse = onMouse;
  }
  render(width) {
    return this.child.render(width);
  }
  handleMouse(event) {
    const childResult = dispatchMouseEvent(this.child, event);
    return childResult ?? this.onMouse(event);
  }
  invalidate() {
    this.child.invalidate();
  }
}

// ---------------------------------------------------------------------------
// Autocomplete (TS autocomplete.ts) and stdin buffering (TS stdin-buffer.ts)
// ---------------------------------------------------------------------------

// TS utils.ts: autocomplete token boundaries.
export const cjkBreakRegex =
	/[\p{Script_Extensions=Han}\p{Script_Extensions=Hiragana}\p{Script_Extensions=Katakana}\p{Script_Extensions=Hangul}\p{Script_Extensions=Bopomofo}]/u;
export const cjkPunctuationRegex = new RegExp(
	`(?:(?=\\p{Punctuation})${cjkBreakRegex.source}|[，．：；！？（）［］｛｝“”‘’…—])`,
	"u",
);
export const autocompleteSeparatorRegex = new RegExp(`(?:\\s|${cjkPunctuationRegex.source})`, "u");
export const autocompleteBoundaryRegex = new RegExp(`(?:^|${autocompleteSeparatorRegex.source})`, "u");

const PATH_DELIMITERS = /* @__PURE__ */ new Set([" ", "	", '"', "'", "="]);
const tokenStartRegex = new RegExp(`${autocompleteBoundaryRegex.source}$`, "u");
const PATH_WRAPPERS = { "(": ")", "[": "]", "{": "}", "<": ">", "`": "`" };
function toDisplayPath(value) {
  return value.replace(/\\/g, "/");
}
function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
function buildFdPathQuery(query) {
  const normalized = toDisplayPath(query);
  if (!normalized.includes("/")) {
    return escapeRegex(normalized);
  }
  const hasTrailingSeparator = normalized.endsWith("/");
  const trimmed = normalized.replace(/^\/+|\/+$/g, "");
  if (!trimmed) {
    return normalized;
  }
  const separatorPattern = "[\\\\/]";
  const segments = trimmed.split("/").filter(Boolean).map((segment) => escapeRegex(segment));
  let pattern = segments.join(separatorPattern);
  if (hasTrailingSeparator) {
    pattern += separatorPattern;
  }
  return pattern;
}
function findLastDelimiter(text) {
  let lastDelimiter = -1;
  let index = 0;
  for (const character of text) {
    index += character.length;
    if (PATH_DELIMITERS.has(character) || autocompleteSeparatorRegex.test(character)) {
      lastDelimiter = index - 1;
    }
  }
  return lastDelimiter;
}
function stripLeadingWrappers(token) {
  let result = token;
  while (result.length > 0) {
    const closer = PATH_WRAPPERS[result[0]];
    if (!closer || result.includes(closer, 1)) {
      break;
    }
    result = result.slice(1);
  }
  return result;
}
function findUnclosedQuoteStart(text) {
  let inQuotes = false;
  let quoteStart = -1;
  for (let i = 0; i < text.length; i += 1) {
    if (text[i] === '"') {
      inQuotes = !inQuotes;
      if (inQuotes) {
        quoteStart = i;
      }
    }
  }
  return inQuotes ? quoteStart : null;
}
function isTokenStart(text, index) {
  let start = index;
  while (start > 0 && PATH_WRAPPERS[text[start - 1]]) {
    start -= 1;
  }
  return PATH_DELIMITERS.has(text[start - 1] ?? "") || tokenStartRegex.test(text.slice(0, start));
}
function extractQuotedPrefix(text) {
  const quoteStart = findUnclosedQuoteStart(text);
  if (quoteStart === null) {
    return null;
  }
  if (quoteStart > 0 && text[quoteStart - 1] === "@") {
    if (!isTokenStart(text, quoteStart - 1)) {
      return null;
    }
    return text.slice(quoteStart - 1);
  }
  if (!isTokenStart(text, quoteStart)) {
    return null;
  }
  return text.slice(quoteStart);
}
function parsePathPrefix(prefix) {
  if (prefix.startsWith('@"')) {
    return { rawPrefix: prefix.slice(2), isAtPrefix: true, isQuotedPrefix: true };
  }
  if (prefix.startsWith('"')) {
    return { rawPrefix: prefix.slice(1), isAtPrefix: false, isQuotedPrefix: true };
  }
  if (prefix.startsWith("@")) {
    return { rawPrefix: prefix.slice(1), isAtPrefix: true, isQuotedPrefix: false };
  }
  return { rawPrefix: prefix, isAtPrefix: false, isQuotedPrefix: false };
}
function buildCompletionValue(path, options) {
  const needsQuotes = options.isQuotedPrefix || autocompleteSeparatorRegex.test(path);
  const prefix = options.isAtPrefix ? "@" : "";
  if (!needsQuotes) {
    return `${prefix}${path}`;
  }
  const openQuote = `${prefix}"`;
  const closeQuote = '"';
  return `${openQuote}${path}${closeQuote}`;
}
async function walkDirectoryWithFd(baseDir, fdPath, query, maxResults, signal, maxDepth) {
  const args = [
    "--base-directory",
    baseDir,
    "--max-results",
    String(maxResults),
    "--type",
    "f",
    "--type",
    "d",
    "--follow",
    "--hidden",
    "--exclude",
    ".git",
    "--exclude",
    ".git/*",
    "--exclude",
    ".git/**"
  ];
  if (maxDepth !== void 0) {
    args.push("--max-depth", String(maxDepth));
  }
  if (toDisplayPath(query).includes("/")) {
    args.push("--full-path");
  }
  if (query) {
    args.push(buildFdPathQuery(query));
  }
  return await new Promise((resolve) => {
    if (signal.aborted) {
      resolve([]);
      return;
    }
    const child = spawn(fdPath, args, {
      stdio: ["ignore", "pipe", "pipe"]
    });
    let stdout = "";
    let resolved = false;
    const finish = (results) => {
      if (resolved) return;
      resolved = true;
      signal.removeEventListener("abort", onAbort);
      resolve(results);
    };
    const onAbort = () => {
      if (child.exitCode === null) {
        child.kill("SIGKILL");
      }
    };
    signal.addEventListener("abort", onAbort, { once: true });
    child.stdout.setEncoding("utf-8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.on("error", () => {
      finish([]);
    });
    child.on("close", (code) => {
      if (signal.aborted || code !== 0 || !stdout) {
        finish([]);
        return;
      }
      const lines = stdout.trim().split("\n").filter(Boolean);
      const results = [];
      for (const line of lines) {
        const displayLine = toDisplayPath(line);
        const hasTrailingSeparator = displayLine.endsWith("/");
        const normalizedPath = hasTrailingSeparator ? displayLine.slice(0, -1) : displayLine;
        if (normalizedPath === ".git" || normalizedPath.startsWith(".git/") || normalizedPath.includes("/.git/")) {
          continue;
        }
        results.push({
          path: displayLine,
          isDirectory: hasTrailingSeparator
        });
      }
      finish(results);
    });
  });
}
export class CombinedAutocompleteProvider {
  commands;
  basePath;
  fdPath;
  constructor(commands = [], basePath, fdPath = null) {
    this.commands = commands;
    this.basePath = basePath;
    this.fdPath = fdPath;
  }
  async getSuggestions(lines, cursorLine, cursorCol, options) {
    const currentLine = lines[cursorLine] || "";
    const textBeforeCursor = currentLine.slice(0, cursorCol);
    const atPrefix = this.extractAtPrefix(textBeforeCursor);
    if (atPrefix) {
      const { rawPrefix, isQuotedPrefix } = parsePathPrefix(atPrefix);
      const suggestions2 = await this.getFuzzyFileSuggestions(rawPrefix, {
        isQuotedPrefix,
        signal: options.signal
      });
      if (suggestions2.length === 0) return null;
      return {
        items: suggestions2,
        prefix: atPrefix
      };
    }
    const commandText = textBeforeCursor.trimStart();
    if (!options.force && commandText.startsWith("/")) {
      const spaceIndex = commandText.indexOf(" ");
      if (spaceIndex === -1) {
        const prefix = commandText.slice(1);
        const commandItems = this.commands.map((cmd) => {
          const name = "name" in cmd ? cmd.name : cmd.value;
          const hint = "argumentHint" in cmd && cmd.argumentHint ? cmd.argumentHint : void 0;
          const desc = cmd.description ?? "";
          const fullDesc = hint ? desc ? `${hint} \u2014 ${desc}` : hint : desc;
          return {
            name,
            label: name,
            description: fullDesc || void 0
          };
        });
        const bareNameMatches = fuzzyFilter(
          commandItems,
          prefix,
          (item) => item.name.startsWith("skill:") ? item.name.slice("skill:".length) : item.name
        );
        const bareNameMatchSet = new Set(bareNameMatches);
        const fullNameOnlyMatches = fuzzyFilter(
          commandItems.filter((item) => item.name.startsWith("skill:") && !bareNameMatchSet.has(item)),
          prefix,
          (item) => item.name
        );
        const filtered = [...bareNameMatches, ...fullNameOnlyMatches].map((item) => ({
          value: item.name,
          label: item.label,
          ...item.description && { description: item.description }
        }));
        if (filtered.length === 0) return null;
        return {
          items: filtered,
          prefix: commandText
        };
      }
      const commandName = commandText.slice(1, spaceIndex);
      const argumentText = commandText.slice(spaceIndex + 1);
      const command = this.commands.find((cmd) => {
        const name = "name" in cmd ? cmd.name : cmd.value;
        return name === commandName;
      });
      if (!command || !("getArgumentCompletions" in command) || !command.getArgumentCompletions) {
        return null;
      }
      const argumentSuggestions = await command.getArgumentCompletions(argumentText);
      if (!Array.isArray(argumentSuggestions) || argumentSuggestions.length === 0) {
        return null;
      }
      return {
        items: argumentSuggestions,
        prefix: argumentText
      };
    }
    const pathMatch = this.extractPathPrefix(textBeforeCursor, options.force ?? false);
    if (pathMatch === null) {
      return null;
    }
    const suggestions = this.getFileSuggestions(pathMatch);
    if (suggestions.length === 0) return null;
    return {
      items: suggestions,
      prefix: pathMatch
    };
  }
  applyCompletion(lines, cursorLine, cursorCol, item, prefix) {
    const currentLine = lines[cursorLine] || "";
    const beforePrefix = currentLine.slice(0, cursorCol - prefix.length);
    const afterCursor = currentLine.slice(cursorCol);
    const isQuotedPrefix = prefix.startsWith('"') || prefix.startsWith('@"');
    const hasLeadingQuoteAfterCursor = afterCursor.startsWith('"');
    const hasTrailingQuoteInItem = item.value.endsWith('"');
    const adjustedAfterCursor = isQuotedPrefix && hasTrailingQuoteInItem && hasLeadingQuoteAfterCursor ? afterCursor.slice(1) : afterCursor;
    const isSlashCommand = prefix.startsWith("/") && beforePrefix.trim() === "" && !prefix.slice(1).includes("/");
    if (isSlashCommand) {
      const newLine2 = `${beforePrefix}/${item.value} ${adjustedAfterCursor}`;
      const newLines2 = [...lines];
      newLines2[cursorLine] = newLine2;
      return {
        lines: newLines2,
        cursorLine,
        cursorCol: beforePrefix.length + item.value.length + 2
        // +2 for "/" and space
      };
    }
    if (prefix.startsWith("@")) {
      const isDirectory2 = item.label.endsWith("/");
      const suffix = isDirectory2 ? "" : " ";
      const newLine2 = `${beforePrefix + item.value}${suffix}${adjustedAfterCursor}`;
      const newLines2 = [...lines];
      newLines2[cursorLine] = newLine2;
      const hasTrailingQuote2 = item.value.endsWith('"');
      const cursorOffset2 = isDirectory2 && hasTrailingQuote2 ? item.value.length - 1 : item.value.length;
      return {
        lines: newLines2,
        cursorLine,
        cursorCol: beforePrefix.length + cursorOffset2 + suffix.length
      };
    }
    const textBeforeCursor = currentLine.slice(0, cursorCol);
    if (textBeforeCursor.includes("/") && textBeforeCursor.includes(" ")) {
      const newLine2 = beforePrefix + item.value + adjustedAfterCursor;
      const newLines2 = [...lines];
      newLines2[cursorLine] = newLine2;
      const isDirectory2 = item.label.endsWith("/");
      const hasTrailingQuote2 = item.value.endsWith('"');
      const cursorOffset2 = isDirectory2 && hasTrailingQuote2 ? item.value.length - 1 : item.value.length;
      return {
        lines: newLines2,
        cursorLine,
        cursorCol: beforePrefix.length + cursorOffset2
      };
    }
    const newLine = beforePrefix + item.value + adjustedAfterCursor;
    const newLines = [...lines];
    newLines[cursorLine] = newLine;
    const isDirectory = item.label.endsWith("/");
    const hasTrailingQuote = item.value.endsWith('"');
    const cursorOffset = isDirectory && hasTrailingQuote ? item.value.length - 1 : item.value.length;
    return {
      lines: newLines,
      cursorLine,
      cursorCol: beforePrefix.length + cursorOffset
    };
  }
  // Extract @ prefix for fuzzy file suggestions
  extractAtPrefix(text) {
    const quotedPrefix = extractQuotedPrefix(text);
    if (quotedPrefix?.startsWith('@"')) {
      return quotedPrefix;
    }
    const lastDelimiterIndex = findLastDelimiter(text);
    const token = stripLeadingWrappers(lastDelimiterIndex === -1 ? text : text.slice(lastDelimiterIndex + 1));
    if (token.startsWith("@")) {
      return token;
    }
    return null;
  }
  // Extract a path-like prefix from the text before cursor
  extractPathPrefix(text, forceExtract = false) {
    const quotedPrefix = extractQuotedPrefix(text);
    if (quotedPrefix) {
      return quotedPrefix;
    }
    const lastDelimiterIndex = findLastDelimiter(text);
    const pathPrefix = stripLeadingWrappers(lastDelimiterIndex === -1 ? text : text.slice(lastDelimiterIndex + 1));
    if (forceExtract) {
      return pathPrefix;
    }
    if (pathPrefix.includes("/") || pathPrefix.startsWith(".") || pathPrefix.startsWith("~/")) {
      return pathPrefix;
    }
    if (pathPrefix === "" && text !== "" && tokenStartRegex.test(text)) {
      return pathPrefix;
    }
    return null;
  }
  // Expand home directory (~/) to actual home path
  expandHomePath(path) {
    if (path.startsWith("~/")) {
      const expandedPath = join(homedir(), path.slice(2));
      return path.endsWith("/") && !expandedPath.endsWith("/") ? `${expandedPath}/` : expandedPath;
    } else if (path === "~") {
      return homedir();
    }
    return path;
  }
  resolveScopedFuzzyQuery(rawQuery) {
    const normalizedQuery = toDisplayPath(rawQuery);
    const slashIndex = normalizedQuery.lastIndexOf("/");
    if (slashIndex === -1) {
      return null;
    }
    const displayBase = normalizedQuery.slice(0, slashIndex + 1);
    const query = normalizedQuery.slice(slashIndex + 1);
    let baseDir;
    if (displayBase.startsWith("~/")) {
      baseDir = this.expandHomePath(displayBase);
    } else if (displayBase.startsWith("/")) {
      baseDir = displayBase;
    } else {
      baseDir = join(this.basePath, displayBase);
    }
    try {
      if (!statSync(baseDir).isDirectory()) {
        return null;
      }
    } catch {
      return null;
    }
    return { baseDir, query, displayBase };
  }
  scopedPathForDisplay(displayBase, relativePath) {
    const normalizedRelativePath = toDisplayPath(relativePath);
    if (displayBase === "/") {
      return `/${normalizedRelativePath}`;
    }
    return `${toDisplayPath(displayBase)}${normalizedRelativePath}`;
  }
  // Get file/directory suggestions for a given path prefix
  getFileSuggestions(prefix) {
    try {
      let searchDir;
      let searchPrefix;
      const { rawPrefix, isAtPrefix, isQuotedPrefix } = parsePathPrefix(prefix);
      let expandedPrefix = rawPrefix;
      if (expandedPrefix.startsWith("~")) {
        expandedPrefix = this.expandHomePath(expandedPrefix);
      }
      const isRootPrefix = rawPrefix === "" || rawPrefix === "./" || rawPrefix === "../" || rawPrefix === "~" || rawPrefix === "~/" || rawPrefix === "/" || isAtPrefix && rawPrefix === "";
      if (isRootPrefix) {
        if (rawPrefix.startsWith("~") || expandedPrefix.startsWith("/")) {
          searchDir = expandedPrefix;
        } else {
          searchDir = join(this.basePath, expandedPrefix);
        }
        searchPrefix = "";
      } else if (rawPrefix.endsWith("/")) {
        if (rawPrefix.startsWith("~") || expandedPrefix.startsWith("/")) {
          searchDir = expandedPrefix;
        } else {
          searchDir = join(this.basePath, expandedPrefix);
        }
        searchPrefix = "";
      } else {
        const dir = dirname(expandedPrefix);
        const file = basename(expandedPrefix);
        if (rawPrefix.startsWith("~") || expandedPrefix.startsWith("/")) {
          searchDir = dir;
        } else {
          searchDir = join(this.basePath, dir);
        }
        searchPrefix = file;
      }
      const entries = readdirSync(searchDir, { withFileTypes: true });
      const suggestions = [];
      for (const entry of entries) {
        if (!entry.name.toLowerCase().startsWith(searchPrefix.toLowerCase())) {
          continue;
        }
        let isDirectory = entry.isDirectory();
        if (!isDirectory && entry.isSymbolicLink()) {
          try {
            const fullPath = join(searchDir, entry.name);
            isDirectory = statSync(fullPath).isDirectory();
          } catch {
          }
        }
        let relativePath;
        const name = entry.name;
        const displayPrefix = rawPrefix;
        if (displayPrefix.endsWith("/")) {
          relativePath = displayPrefix + name;
        } else if (displayPrefix.includes("/") || displayPrefix.includes("\\")) {
          if (displayPrefix.startsWith("~/")) {
            const homeRelativeDir = displayPrefix.slice(2);
            const dir = dirname(homeRelativeDir);
            relativePath = `~/${dir === "." ? name : join(dir, name)}`;
          } else if (displayPrefix.startsWith("/")) {
            const dir = dirname(displayPrefix);
            if (dir === "/") {
              relativePath = `/${name}`;
            } else {
              relativePath = `${dir}/${name}`;
            }
          } else {
            relativePath = join(dirname(displayPrefix), name);
            if (displayPrefix.startsWith("./") && !relativePath.startsWith("./")) {
              relativePath = `./${relativePath}`;
            }
          }
        } else {
          if (displayPrefix.startsWith("~")) {
            relativePath = `~/${name}`;
          } else {
            relativePath = name;
          }
        }
        relativePath = toDisplayPath(relativePath);
        const pathValue = isDirectory ? `${relativePath}/` : relativePath;
        const value = buildCompletionValue(pathValue, {
          isDirectory,
          isAtPrefix,
          isQuotedPrefix
        });
        suggestions.push({
          value,
          label: name + (isDirectory ? "/" : "")
        });
      }
      suggestions.sort((a, b) => {
        const aIsDir = a.label.endsWith("/");
        const bIsDir = b.label.endsWith("/");
        if (aIsDir && !bIsDir) return -1;
        if (!aIsDir && bIsDir) return 1;
        return a.label.localeCompare(b.label);
      });
      return suggestions;
    } catch (_e) {
      return [];
    }
  }
  // Score an entry against the query (higher = better match)
  // isDirectory adds bonus to prioritize folders
  scoreEntry(filePath, query, isDirectory) {
    const fileName = basename(filePath);
    const lowerFileName = fileName.toLowerCase();
    const lowerQuery = query.toLowerCase();
    let score = 0;
    if (lowerFileName === lowerQuery) score = 100;
    else if (lowerFileName.startsWith(lowerQuery)) score = 80;
    else if (lowerFileName.includes(lowerQuery)) score = 50;
    else if (filePath.toLowerCase().includes(lowerQuery)) score = 30;
    if (isDirectory && score > 0) score += 10;
    return score;
  }
  async getBaseDirSuggestions(baseDir, query, signal) {
    if (!this.fdPath || signal.aborted) {
      return [];
    }
    return await walkDirectoryWithFd(baseDir, this.fdPath, query, 100, signal, 1);
  }
  // Fuzzy file search using fd (fast, respects .gitignore)
  async getFuzzyFileSuggestions(query, options) {
    if (!this.fdPath || options.signal.aborted) {
      return [];
    }
    try {
      const scopedQuery = this.resolveScopedFuzzyQuery(query);
      const fdBaseDir = scopedQuery?.baseDir ?? this.basePath;
      const fdQuery = scopedQuery?.query ?? query;
      const baseDirEntries = await this.getBaseDirSuggestions(fdBaseDir, fdQuery, options.signal);
      const recursiveEntries = await walkDirectoryWithFd(fdBaseDir, this.fdPath, fdQuery, 100, options.signal);
      const seenPaths = new Set(baseDirEntries.map((entry) => entry.path));
      const entries = [
        ...baseDirEntries,
        ...recursiveEntries.filter((entry) => {
          if (seenPaths.has(entry.path)) return false;
          seenPaths.add(entry.path);
          return true;
        })
      ];
      if (options.signal.aborted) {
        return [];
      }
      const scoredEntries = entries.map((entry) => ({
        ...entry,
        score: fdQuery ? this.scoreEntry(entry.path, fdQuery, entry.isDirectory) : 1
      })).filter((entry) => entry.score > 0);
      scoredEntries.sort((a, b) => {
        const scoreDiff = b.score - a.score;
        if (scoreDiff !== 0) return scoreDiff;
        const aDepth = toDisplayPath(a.path).split("/").filter(Boolean).length;
        const bDepth = toDisplayPath(b.path).split("/").filter(Boolean).length;
        const depthDiff = aDepth - bDepth;
        if (depthDiff !== 0) return depthDiff;
        const lengthDiff = a.path.length - b.path.length;
        if (lengthDiff !== 0) return lengthDiff;
        return a.path.localeCompare(b.path);
      });
      const topEntries = scoredEntries.slice(0, 20);
      const suggestions = [];
      for (const { path: entryPath, isDirectory } of topEntries) {
        const pathWithoutSlash = isDirectory ? entryPath.slice(0, -1) : entryPath;
        const displayPath = scopedQuery ? this.scopedPathForDisplay(scopedQuery.displayBase, pathWithoutSlash) : pathWithoutSlash;
        const entryName = basename(pathWithoutSlash);
        const completionPath = isDirectory ? `${displayPath}/` : displayPath;
        const value = buildCompletionValue(completionPath, {
          isDirectory,
          isAtPrefix: true,
          isQuotedPrefix: options.isQuotedPrefix
        });
        suggestions.push({
          value,
          label: entryName + (isDirectory ? "/" : ""),
          description: displayPath
        });
      }
      return suggestions;
    } catch {
      return [];
    }
  }
  // Check if we should trigger file completion (called on Tab key)
  shouldTriggerFileCompletion(lines, cursorLine, cursorCol) {
    const currentLine = lines[cursorLine] || "";
    const textBeforeCursor = currentLine.slice(0, cursorCol);
    if (textBeforeCursor.trim().startsWith("/") && !textBeforeCursor.trim().includes(" ")) {
      return false;
    }
    return true;
  }
}

const ESC = "\x1B";
const DEFAULT_SEQUENCE_TIMEOUT_MS = 50;
const DEFAULT_ESCAPE_TIMEOUT_MS = 10;
const BRACKETED_PASTE_START = "\x1B[200~";
const BRACKETED_PASTE_END = "\x1B[201~";
const TERMINAL_REPLY_HEADER_PATTERN = /^\x1b(?:\](?:1[01];|4;\d{1,3};|7501;\?)|P>\||_G(?:[a-z]=\d+,)*[a-z]=\d+;)/;
function isControlString(data) {
  return data.startsWith(`${ESC}]`) || data.startsWith(`${ESC}P`) || data.startsWith(`${ESC}_`);
}
function isCompleteSequence(data) {
  if (!data.startsWith(ESC)) {
    return "not-escape";
  }
  if (data.length === 1) {
    return "incomplete";
  }
  const afterEsc = data.slice(1);
  if (afterEsc.startsWith("[")) {
    if (afterEsc.startsWith("[M")) {
      return data.length >= 6 ? "complete" : "incomplete";
    }
    return isCompleteCsiSequence(data);
  }
  if (afterEsc.startsWith("]")) {
    return isCompleteOscSequence(data);
  }
  if (afterEsc.startsWith("P")) {
    return isCompleteDcsSequence(data);
  }
  if (afterEsc.startsWith("_")) {
    return isCompleteApcSequence(data);
  }
  if (afterEsc.startsWith("O")) {
    return afterEsc.length >= 2 ? "complete" : "incomplete";
  }
  if (afterEsc.length === 1) {
    return "complete";
  }
  return "complete";
}
function isCompleteCsiSequence(data) {
  if (!data.startsWith(`${ESC}[`)) {
    return "complete";
  }
  if (data.length < 3) {
    return "incomplete";
  }
  const payload = data.slice(2);
  const lastChar = payload[payload.length - 1];
  const lastCharCode = lastChar.charCodeAt(0);
  if (lastCharCode >= 64 && lastCharCode <= 126) {
    if (payload.startsWith("<")) {
      const mouseMatch = /^<\d+;\d+;\d+[Mm]$/.test(payload);
      if (mouseMatch) {
        return "complete";
      }
      if (lastChar === "M" || lastChar === "m") {
        const parts = payload.slice(1, -1).split(";");
        if (parts.length === 3 && parts.every((p) => /^\d+$/.test(p))) {
          return "complete";
        }
      }
      return "incomplete";
    }
    return "complete";
  }
  return "incomplete";
}
function isCompleteOscSequence(data) {
  if (!data.startsWith(`${ESC}]`)) {
    return "complete";
  }
  if (data.endsWith(`${ESC}\\`) || data.endsWith("\x07")) {
    return "complete";
  }
  return "incomplete";
}
function isCompleteDcsSequence(data) {
  if (!data.startsWith(`${ESC}P`)) {
    return "complete";
  }
  if (data.endsWith(`${ESC}\\`)) {
    return "complete";
  }
  return "incomplete";
}
function isCompleteApcSequence(data) {
  if (!data.startsWith(`${ESC}_`)) {
    return "complete";
  }
  if (data.endsWith(`${ESC}\\`)) {
    return "complete";
  }
  return "incomplete";
}
function parseUnmodifiedKittyPrintableCodepoint(sequence) {
  const match = sequence.match(/^\x1b\[(\d+)(?::\d*)?(?::\d+)?u$/);
  if (!match) return void 0;
  const codepoint = parseInt(match[1], 10);
  return codepoint >= 32 ? codepoint : void 0;
}
function extractCompleteSequences(buffer) {
  const sequences = [];
  let pos = 0;
  while (pos < buffer.length) {
    const remaining = buffer.slice(pos);
    if (remaining.startsWith(ESC)) {
      const terminalReply = isControlString(remaining) && TERMINAL_REPLY_HEADER_PATTERN.test(remaining);
      let seqEnd = 1;
      while (seqEnd <= remaining.length) {
        if (terminalReply && seqEnd > 2) {
          const char = remaining[seqEnd - 1];
          if (char === "" || char === "") {
            pos += seqEnd;
            break;
          }
          if (char === ESC && seqEnd < remaining.length && remaining[seqEnd] !== "\\") {
            pos += seqEnd - 1;
            break;
          }
        }
        const candidate = remaining.slice(0, seqEnd);
        const status = isCompleteSequence(candidate);
        if (status === "complete") {
          if (candidate === "\x1B\x1B") {
            const nextChar = remaining[seqEnd];
            if (nextChar === "[" || // CSI
            nextChar === "]" || // OSC
            nextChar === "O" || // SS3
            nextChar === "P" || // DCS
            nextChar === "_") {
              sequences.push(ESC);
              pos += 1;
              break;
            }
          }
          sequences.push(candidate);
          pos += seqEnd;
          break;
        } else if (status === "incomplete") {
          seqEnd++;
        } else {
          sequences.push(candidate);
          pos += seqEnd;
          break;
        }
      }
      if (seqEnd > remaining.length) {
        return { sequences, remainder: remaining };
      }
    } else {
      sequences.push(remaining[0]);
      pos++;
    }
  }
  return { sequences, remainder: "" };
}
export class StdinBuffer extends EventEmitter {
  buffer = "";
  timeout = null;
  timeoutMs;
  escapeTimeoutMs;
  pasteMode = false;
  pasteBuffer = "";
  pendingKittyPrintableCodepoint;
  constructor(options = {}) {
    super();
    this.timeoutMs = options.timeout ?? DEFAULT_SEQUENCE_TIMEOUT_MS;
    this.escapeTimeoutMs = options.escapeTimeout ?? DEFAULT_ESCAPE_TIMEOUT_MS;
  }
  process(data) {
    if (this.timeout) {
      clearTimeout(this.timeout);
      this.timeout = null;
    }
    let str;
    if (Buffer.isBuffer(data)) {
      if (data.length === 1 && data[0] > 127) {
        const byte = data[0] - 128;
        str = `\x1B${String.fromCharCode(byte)}`;
      } else {
        str = data.toString();
      }
    } else {
      str = data;
    }
    if (str.length === 0 && this.buffer.length === 0) {
      this.emitDataSequence("");
      return;
    }
    this.buffer += str;
    if (this.pasteMode) {
      this.pasteBuffer += this.buffer;
      this.buffer = "";
      const endIndex = this.pasteBuffer.indexOf(BRACKETED_PASTE_END);
      if (endIndex !== -1) {
        const pastedContent = this.pasteBuffer.slice(0, endIndex);
        const remaining = this.pasteBuffer.slice(endIndex + BRACKETED_PASTE_END.length);
        this.pasteMode = false;
        this.pasteBuffer = "";
        this.pendingKittyPrintableCodepoint = void 0;
        this.emit("paste", pastedContent);
        if (remaining.length > 0) {
          this.process(remaining);
        }
      }
      return;
    }
    const startIndex = this.buffer.indexOf(BRACKETED_PASTE_START);
    if (startIndex !== -1) {
      if (startIndex > 0) {
        const beforePaste = this.buffer.slice(0, startIndex);
        const result2 = extractCompleteSequences(beforePaste);
        for (const sequence of result2.sequences) {
          this.emitDataSequence(sequence);
        }
      }
      this.pendingKittyPrintableCodepoint = void 0;
      this.buffer = this.buffer.slice(startIndex + BRACKETED_PASTE_START.length);
      this.pasteMode = true;
      this.pasteBuffer = this.buffer;
      this.buffer = "";
      const endIndex = this.pasteBuffer.indexOf(BRACKETED_PASTE_END);
      if (endIndex !== -1) {
        const pastedContent = this.pasteBuffer.slice(0, endIndex);
        const remaining = this.pasteBuffer.slice(endIndex + BRACKETED_PASTE_END.length);
        this.pasteMode = false;
        this.pasteBuffer = "";
        this.pendingKittyPrintableCodepoint = void 0;
        this.emit("paste", pastedContent);
        if (remaining.length > 0) {
          this.process(remaining);
        }
      }
      return;
    }
    const result = extractCompleteSequences(this.buffer);
    this.buffer = result.remainder;
    for (const sequence of result.sequences) {
      this.emitDataSequence(sequence);
    }
    if (this.buffer.length > 0) {
      if (TERMINAL_REPLY_HEADER_PATTERN.test(this.buffer)) return;
      const timeoutMs = this.buffer === ESC ? this.escapeTimeoutMs : this.timeoutMs;
      this.timeout = setTimeout(() => {
        const flushed = this.flush();
        for (const sequence of flushed) {
          if (sequence.length > 2 && isControlString(sequence)) {
            this.emitDataSequence(sequence.slice(0, 2));
            this.process(sequence.slice(2));
          } else {
            this.emitDataSequence(sequence);
          }
        }
      }, timeoutMs);
    }
  }
  emitDataSequence(sequence) {
    const rawCodepoint = sequence.length === 1 ? sequence.codePointAt(0) : void 0;
    if (rawCodepoint !== void 0 && rawCodepoint === this.pendingKittyPrintableCodepoint) {
      this.pendingKittyPrintableCodepoint = void 0;
      return;
    }
    this.pendingKittyPrintableCodepoint = parseUnmodifiedKittyPrintableCodepoint(sequence);
    this.emit("data", sequence);
  }
  flush() {
    if (this.timeout) {
      clearTimeout(this.timeout);
      this.timeout = null;
    }
    if (this.buffer.length === 0) {
      return [];
    }
    const sequences = [this.buffer];
    this.buffer = "";
    this.pendingKittyPrintableCodepoint = void 0;
    return sequences;
  }
  clear() {
    if (this.timeout) {
      clearTimeout(this.timeout);
      this.timeout = null;
    }
    this.buffer = "";
    this.pasteMode = false;
    this.pasteBuffer = "";
    this.pendingKittyPrintableCodepoint = void 0;
  }
  getBuffer() {
    return this.buffer;
  }
  destroy() {
    this.clear();
  }
}

// ---------------------------------------------------------------------------
// Host-only classes (not available to extensions in pi-rs)
// ---------------------------------------------------------------------------

function unavailable(name) {
	return class {
		constructor() {
			throw new Error(`${name} is not available to extensions in pi-rs`);
		}
	};
}

export const TUI = unavailable("TUI");
export const TuiMainScreen = unavailable("TuiMainScreen");
export const ProcessTerminal = unavailable("ProcessTerminal");

export const isAppleTerminalSession = () => util("isAppleTerminalSession");
// TS loads a native clipboard addon (macOS, Windows, Linux with X11 `DISPLAY`); pi-rs uses pi-tui's
// arboard-backed helper in the same places. Undefined means unavailable, null means no data of that
// kind; transfer failures reject.
function clipboardResult(result) {
	if (!result || result.kind === "unavailable") return undefined;
	if (result.kind === "empty") return null;
	return result.data;
}
let nativeClipboard;
export function getNativeClipboard() {
	const support = util("nativeClipboard");
	if (!support) return undefined;
	nativeClipboard ??= {
		async getText() {
			return clipboardResult(await callAsync("tui.clipboard", { m: "getText" }));
		},
		async getImage() {
			const data = clipboardResult(await callAsync("tui.clipboard", { m: "getImage" }));
			return typeof data === "string" ? new Uint8Array(Buffer.from(data, "base64")) : data;
		},
		async getFilePaths() {
			return clipboardResult(await callAsync("tui.clipboard", { m: "getFilePaths" }));
		},
	};
	if (support.setText && !nativeClipboard.setText) {
		nativeClipboard.setText = async (text) => {
			await callAsync("tui.clipboard", { m: "setText", text: String(text) });
		};
	}
	return nativeClipboard;
}
export const renderLatex = (source, options = {}) => util("renderLatex", String(source), options) ?? undefined;
export const formatProgramStatus = (status) => util("formatProgramStatus", status);
export const styleText = (text, options, mode) => util("styleText", String(text), options ?? {}, mode);
export const styleTextWithAnsi = (text, fgAnsi, bgAnsi, options) =>
	util("styleTextWithAnsi", String(text), fgAnsi ?? null, bgAnsi ?? null, options ?? {});


// ---------------------------------------------------------------------------
// Colors and terminal images (pi_tui::colors / pi_tui::terminal_image)
// ---------------------------------------------------------------------------

export const indexedColor = (...args) => util("indexedColor", ...args);
export const rgbColor = (...args) => util("rgbColor", ...args);
export const oklchColor = (...args) => util("oklchColor", ...args);
export const okhslColor = (...args) => util("okhslColor", ...args);
export const parseColor = (...args) => util("parseColor", ...args);
export const colorToRgb = (...args) => util("colorToRgb", ...args);
export const colorToOklch = (...args) => util("colorToOklch", ...args);
export const colorToOkhsl = (...args) => util("colorToOkhsl", ...args);
export const colorToHex = (...args) => util("colorToHex", ...args);
export const mixColors = (...args) => util("mixColors", ...args);
export const foregroundAnsi = (...args) => util("foregroundAnsi", ...args);
export const backgroundAnsi = (...args) => util("backgroundAnsi", ...args);
export const oklabToOkhslLightness = (...args) => util("oklabToOkhslLightness", ...args);
export const parseTerminalColorSchemeReport = (...args) => util("parseTerminalColorSchemeReport", ...args) ?? undefined;
export const getOsc8LinkAtColumn = (...args) => util("getOsc8LinkAtColumn", ...args) ?? undefined;
export const compositeTuiLine = (...args) => util("compositeTuiLine", ...args);
export const getCellDimensions = (...args) => util("getCellDimensions", ...args);
export const setCellDimensions = (...args) => util("setCellDimensions", ...args);
export const detectCapabilities = (...args) => util("detectCapabilities", ...args);
export const getCapabilities = (...args) => util("getCapabilities", ...args);
export const setCapabilities = (...args) => util("setCapabilities", ...args);
export const resetCapabilitiesCache = (...args) => util("resetCapabilitiesCache", ...args);
export const setCapabilityOverrides = (...args) => util("setCapabilityOverrides", ...args);
export const getTerminalColorMode = (...args) => util("getTerminalColorMode", ...args);
export const allocateImageId = (...args) => util("allocateImageId", ...args);
export const encodeKitty = (...args) => util("encodeKitty", ...args);
export const encodeITerm2 = (...args) => util("encodeITerm2", ...args);
export const deleteKittyImage = (...args) => util("deleteKittyImage", ...args);
export const deleteAllKittyImages = (...args) => util("deleteAllKittyImages", ...args);
export const calculateImageRows = (...args) => util("calculateImageRows", ...args);
export const getPngDimensions = (...args) => util("getPngDimensions", ...args) ?? undefined;
export const getJpegDimensions = (...args) => util("getJpegDimensions", ...args) ?? undefined;
export const getGifDimensions = (...args) => util("getGifDimensions", ...args) ?? undefined;
export const getWebpDimensions = (...args) => util("getWebpDimensions", ...args) ?? undefined;
export const getImageDimensions = (...args) => util("getImageDimensions", ...args) ?? undefined;
export const renderImage = (...args) => util("renderImage", ...args) ?? undefined;
export const hyperlink = (...args) => util("hyperlink", ...args);
export const imageFallback = (...args) => util("imageFallback", ...args);

export function isViewportTUI(tui) {
	return tui?.[VIEWPORT_TUI] === true;
}

/** The alternate-screen TUI is created by pi's interactive mode; extensions use the TUI they are given. */
export class TuiAltScreen {
	constructor() {
		throw new Error("TuiAltScreen is not available to extensions in pi-rs");
	}
}

export { Marked } from "pi-rs:marked";

export { tuiProxy as __tuiProxy, componentRef as __componentRef, rustCallback as __rustCallback, markdownThemeRef as __markdownThemeRef };
