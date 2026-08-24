window.__ModuleLoader__.load({
	id: 'dsh-nyro-usage',
	factory: (require) => {
		var module = { exports: {} };
		var exports = module.exports;
Object.defineProperty(exports, Symbol.toStringTag, { value: "Module" });
let react_dom_client = require("react-dom/client");
let react = require("react");
let react_jsx_runtime = require("react/jsx-runtime");
let _deepseek_ai_dsh_client_runtime_client = require("@deepseek-ai/dsh-client-runtime/client");
//#region src/client/api.ts
/** Error carrying the route's JSON error message. */
var NyroPanelApiError = class extends Error {
	kind;
	constructor(message, kind) {
		super(message);
		this.kind = kind;
		this.name = "NyroPanelApiError";
	}
};
/** Parse a JSON response or throw a NyroPanelApiError. */
async function readJson(response) {
	let body;
	try {
		body = await response.json();
	} catch {
		throw new NyroPanelApiError(`HTTP ${response.status}: invalid JSON response`);
	}
	if (!response.ok) {
		const record = typeof body === "object" && body !== null ? body : {};
		throw new NyroPanelApiError(typeof record.error === "string" && record.error !== "" ? record.error : `HTTP ${response.status}`, typeof record.kind === "string" ? record.kind : void 0);
	}
	return body;
}
/** The host proxy routes (same origin). */
var NyroPanelApi = class {
	/** Sanitized config + cache state. */
	async status() {
		return readJson(await fetch("/api/nyro-usage/status"));
	}
	/** Every provider's usage; `refresh` bypasses the host-side cache. */
	async usage(refresh = false) {
		return readJson(await fetch(`/api/nyro-usage/usage${refresh ? "?refresh=1" : ""}`));
	}
	/** Connectivity + auth test against the saved configuration. */
	async test() {
		return readJson(await fetch("/api/nyro-usage/test", { method: "POST" }));
	}
};
//#endregion
//#region src/client/locales.ts
/**
* dsh-nyro-usage surface copy: zh is the key source, en mirrors every key.
*/
const zh = {
	"entry.label": "Nyro 用量",
	"entry.tooltip": "nyro 网关 provider 用量面板",
	"panel.title": "Nyro 用量",
	"panel.refresh": "刷新",
	"panel.refreshing": "刷新中…",
	"panel.updated": "更新于 {ago}",
	"panel.autoRefresh": "自动刷新",
	"panel.autoRefresh.off": "关闭",
	"panel.summary": "{ok} 正常 · {exhausted} 配额耗尽",
	"panel.notConfigured": "尚未配置 nyro 连接",
	"panel.notConfiguredHint": "在「设置 → 插件 → Nyro 用量」中填写 baseUrl 与 adminToken，保存后回到本面板。",
	"panel.loadError": "加载失败",
	"panel.retry": "重试",
	"panel.empty": "nyro 中没有 provider。",
	"panel.cached": "缓存",
	"panel.resetOrder": "恢复默认排序",
	"card.disabled": "已停用",
	"card.error": "查询失败",
	"card.eligible": "可调度",
	"card.quotaExhausted": "配额耗尽",
	"card.balance": "余额",
	"card.spendToday": "今日",
	"card.spendMonth": "本月",
	"card.queriedAt": "查询于 {ago}",
	"card.dragHandle": "按住拖动可调整卡片顺序",
	"tier.five_hour": "5小时",
	"tier.weekly_limit": "每周",
	"tier.monthly": "每月",
	"tier.primary_window": "主要窗口",
	"tier.secondary_window": "次要窗口",
	"tier.feature": "{feature} · {window}",
	"tier.pace": "匀速参考线：{percent}%（按整窗匀速使用推进）",
	"settings.title": "Nyro 用量",
	"settings.description": "nyro 网关 provider 用量面板的连接与刷新设置",
	"settings.enabled": "启用插件",
	"settings.enabledHint": "关闭后卸载用量代理路由，面板不再加载数据。",
	"settings.baseUrl": "nyro Base URL",
	"settings.baseUrlHint": "nyro 管理面地址，例如 http://192.168.31.2:19531（结尾的 /api/v1 可省略）。",
	"settings.adminToken": "Admin Token",
	"settings.adminTokenHint": "nyro 的 NYRO_ADMIN_TOKEN（Bearer 鉴权）。保存后回读脱敏，留空表示不修改。",
	"settings.refreshSeconds": "自动刷新间隔（秒）",
	"settings.refreshSecondsHint": "面板自动刷新的最小间隔，不低于 15 秒。",
	"settings.cacheSeconds": "缓存 TTL（秒）",
	"settings.cacheSecondsHint": "host 侧缓存用量结果的时长；nyro 每次查询都会实时请求上游，缓存可避免频繁刷新打爆上游。0 表示关闭缓存。",
	"settings.test": "测试连接",
	"settings.testing": "测试中…",
	"settings.testOk": "连接成功（{count} 个 provider）",
	"settings.testFail": "连接失败：{message}",
	"settings.collapse": "收起",
	"settings.expand": "展开",
	"settings.notExposed": "当前部署未向浏览器开放此插件的设置命名空间。",
	"settings.unsaved": "有未保存的修改",
	"settings.readOnly": "当前文档为只读，无法保存修改。",
	"settings.saveFailed": "保存失败",
	"settings.discard": "放弃修改",
	"settings.save": "保存",
	"settings.saving": "保存中…",
	"settings.overridden": "已覆盖",
	"settings.reset": "重置",
	"settings.invalidNumber": "不是有效的数值",
	"settings.inherit": "继承默认",
	"settings.on": "开",
	"settings.off": "关"
};
const en = {
	"entry.label": "Nyro Usage",
	"entry.tooltip": "Provider usage panel for the nyro gateway",
	"panel.title": "Nyro Usage",
	"panel.refresh": "Refresh",
	"panel.refreshing": "Refreshing…",
	"panel.updated": "Updated {ago}",
	"panel.autoRefresh": "Auto refresh",
	"panel.autoRefresh.off": "Off",
	"panel.summary": "{ok} ok · {exhausted} exhausted",
	"panel.notConfigured": "nyro connection is not configured",
	"panel.notConfiguredHint": "Fill in baseUrl and adminToken under \"Settings → Plugins → Nyro Usage\", save, then come back to this panel.",
	"panel.loadError": "Failed to load",
	"panel.retry": "Retry",
	"panel.empty": "No providers configured in nyro.",
	"panel.cached": "cached",
	"panel.resetOrder": "Reset order",
	"card.disabled": "Disabled",
	"card.error": "Query failed",
	"card.eligible": "Eligible",
	"card.quotaExhausted": "Quota exhausted",
	"card.balance": "Balance",
	"card.spendToday": "Today",
	"card.spendMonth": "Month",
	"card.queriedAt": "Queried {ago}",
	"card.dragHandle": "Hold and drag to reorder cards",
	"tier.five_hour": "5h",
	"tier.weekly_limit": "Weekly",
	"tier.monthly": "Monthly",
	"tier.primary_window": "Primary window",
	"tier.secondary_window": "Secondary window",
	"tier.feature": "{feature} · {window}",
	"tier.pace": "Steady pace: {percent}% (even consumption over the window)",
	"settings.title": "Nyro Usage",
	"settings.description": "Connection and refresh settings for the nyro provider usage panel",
	"settings.enabled": "Enable plugin",
	"settings.enabledHint": "When off, the usage proxy routes are dropped and the panel loads no data.",
	"settings.baseUrl": "nyro Base URL",
	"settings.baseUrlHint": "nyro admin-plane address, e.g. http://192.168.31.2:19531 (a trailing /api/v1 is tolerated).",
	"settings.adminToken": "Admin Token",
	"settings.adminTokenHint": "nyro's NYRO_ADMIN_TOKEN (Bearer auth). Redacted on read-back once saved; leave empty to keep it.",
	"settings.refreshSeconds": "Auto refresh (seconds)",
	"settings.refreshSecondsHint": "Minimum panel auto-refresh interval, at least 15 seconds.",
	"settings.cacheSeconds": "Cache TTL (seconds)",
	"settings.cacheSecondsHint": "How long the host caches usage results; nyro queries its upstreams live on every call, so the cache keeps frequent refreshes polite. 0 disables caching.",
	"settings.test": "Test connection",
	"settings.testing": "Testing…",
	"settings.testOk": "Connected ({count} providers)",
	"settings.testFail": "Connection failed: {message}",
	"settings.collapse": "Collapse",
	"settings.expand": "Expand",
	"settings.notExposed": "This deployment does not serve the plugin settings namespace to the browser.",
	"settings.unsaved": "Unsaved changes",
	"settings.readOnly": "The current document is read-only; changes cannot be saved.",
	"settings.saveFailed": "Save failed",
	"settings.discard": "Discard",
	"settings.save": "Save",
	"settings.saving": "Saving…",
	"settings.overridden": "Overridden",
	"settings.reset": "Reset",
	"settings.invalidNumber": "Not a valid number",
	"settings.inherit": "Inherit default",
	"settings.on": "On",
	"settings.off": "Off"
};
/** Interpolate `{param}` placeholders in a locale string. */
function format(template, params) {
	return template.replace(/\{(\w+)\}/g, (match, key) => Object.hasOwn(params, key) ? String(params[key]) : match);
}
//#endregion
//#region src/client/tt.ts
/**
* Shared panel helpers: the active-dictionary pick (document-language
* based, task-board / dsh-ssh precedent) bound to the plugin's
* interpolator, plus a small error-message extractor.
*/
/** Active dictionary, picked by the document language at call time. */
function dictionary() {
	return (typeof document !== "undefined" ? document.documentElement.lang : "zh").toLowerCase().startsWith("en") ? { ...en } : { ...zh };
}
/** Translate a key with optional {name} template params (current language). */
function tt(key, values) {
	const text = dictionary()[key] ?? key;
	return values === void 0 ? text : format(text, values);
}
/** Human-readable error text from an unknown thrown value. */
function errorMessage(error) {
	if (error instanceof Error) return error.message;
	return String(error);
}
/** Compact relative age like "2h30m" / "3d12h" / "45s". */
function agoLabel(timestamp) {
	if (timestamp === null || timestamp === void 0) return "";
	const diffMs = Date.now() - new Date(timestamp).getTime();
	if (!Number.isFinite(diffMs)) return "";
	if (diffMs < 0) return "0s";
	const seconds = Math.floor(diffMs / 1e3);
	if (seconds < 60) return `${seconds}s`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) return `${minutes}m`;
	const hours = Math.floor(minutes / 60);
	if (hours < 24) return `${hours}h${minutes % 60}m`;
	return `${Math.floor(hours / 24)}d${hours % 24}h`;
}
/** Compact countdown like "2h30m" / "3d12h"; empty when already past. */
function countdownLabel(resetsAt) {
	if (resetsAt === null || resetsAt === void 0) return "";
	const diffMs = new Date(resetsAt).getTime() - Date.now();
	if (!Number.isFinite(diffMs) || diffMs <= 0) return "";
	const seconds = Math.floor(diffMs / 1e3);
	if (seconds < 60) return `${seconds}s`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) return `${minutes}m`;
	const hours = Math.floor(minutes / 60);
	if (hours < 24) return `${hours}h${minutes % 60}m`;
	return `${Math.floor(hours / 24)}d${hours % 24}h`;
}
//#endregion
//#region src/client/panel/cardOrder.ts
/**
* Provider-card ordering: drag-to-reorder positions persisted per browser
* in localStorage (task-board `dsh.<plugin>.<feature>.vN` precedent).
* Provider ids unknown to the saved order — added in nyro after the last
* drag — keep the gateway's natural order, appended after every explicitly
* ordered card, so new providers never shuffle existing positions.
*/
/** localStorage slot holding the saved provider order (array of ids). */
const ORDER_KEY = "dsh.nyroUsage.cardOrder.v1";
/** Read the saved order; any corruption falls back to "no custom order". */
function loadOrder() {
	try {
		const raw = window.localStorage.getItem(ORDER_KEY);
		if (raw === null) return [];
		const parsed = JSON.parse(raw);
		if (!Array.isArray(parsed)) return [];
		return [...new Set(parsed.filter((id) => typeof id === "string"))];
	} catch {
		return [];
	}
}
/** Persist (or clear) the order; storage failures just don't persist. */
function saveOrder(ids) {
	try {
		if (ids.length === 0) window.localStorage.removeItem(ORDER_KEY);
		else window.localStorage.setItem(ORDER_KEY, JSON.stringify(ids));
	} catch {}
}
/**
* Sort items by the saved id order. `Array#sort` is stable, so ids missing
* from `order` keep their incoming relative order after the ordered ones.
*/
function bySavedOrder(items, order, idOf) {
	if (order.length === 0) return items;
	const rank = new Map(order.map((id, index) => [id, index]));
	return [...items].sort((a, b) => {
		return (rank.get(idOf(a)) ?? order.length) - (rank.get(idOf(b)) ?? order.length);
	});
}
/** Move `dragId` to before/after `overId` inside `ids`; an unknown anchor
* appends at the end (the whole displayed sequence is what gets saved). */
function moved(ids, dragId, overId, before) {
	const next = ids.filter((id) => id !== dragId);
	const index = next.indexOf(overId);
	next.splice(index === -1 ? next.length : before ? index : index + 1, 0, dragId);
	return next;
}
/** State hook backing the draggable card order. */
function useCardOrder() {
	const [order, setOrder] = (0, react.useState)(loadOrder);
	const commit = (0, react.useCallback)((next) => {
		setOrder(next);
		saveOrder(next);
	}, []);
	return {
		order,
		move: (0, react.useCallback)((displayed, dragId, overId, before) => {
			commit(moved(displayed, dragId, overId, before));
		}, [commit]),
		append: (0, react.useCallback)((displayed, dragId) => {
			commit([...displayed.filter((id) => id !== dragId), dragId]);
		}, [commit]),
		reset: (0, react.useCallback)(() => {
			commit([]);
		}, [commit])
	};
}
//#endregion
//#region \0dsh-nyro-usage-css:/home/ubuntu/code/dsh-nyro-usage/src/client/panel/panel.module.css.js
const cssText$1 = "/**\n * nyro usage panel styles. Scoped by the plugin's own data attributes so\n * nothing leaks into the rest of the GUI; colors ride the dsh --dsw-* tokens\n * so the panel follows the active theme (light/dark and skins). The\n * center-column hide rules are attribute-scoped and must stay in this\n * stylesheet (it is imported by mount.tsx, so the styles load with the\n * plugin).\n *\n * Class names get the `nyu-` prefix at build time (see tsdown.config.ts).\n */\n\n/* --- center-column takeover (global rules, attribute-scoped) ---------------- */\n\n[data-pane='conversation'],\n[class*='centerCol'] {\n  position: relative;\n}\n\n/* The panel container rides inside the conversation grid item as an extra\n   trailing child; hidden unless the panel is active. */\n[data-dsh-nyro-usage-view] {\n  position: absolute;\n  inset: 0;\n  display: none;\n  z-index: 60;\n  background: var(--dsw-alias-bg-base);\n  overflow: hidden;\n}\n\nhtml[data-dsh-nyro-usage-active]:not([data-dsh-taskboard-active]):not([data-dsh-ssh-active]) [data-dsh-nyro-usage-view] {\n  display: block;\n}\n\nhtml[data-dsh-nyro-usage-active]:not([data-dsh-taskboard-active]):not([data-dsh-ssh-active]) [data-pane='conversation'] > :not([data-dsh-nyro-usage-view]),\nhtml[data-dsh-nyro-usage-active]:not([data-dsh-taskboard-active]):not([data-dsh-ssh-active]) [class*='centerCol'] > :not([data-dsh-nyro-usage-view]) {\n  display: none !important;\n}\n\n/* --- sidebar entry (mirrors the shell's nav-item look) ---------------------- */\n\n.nyu-entry {\n  width: 100%;\n  height: 32px;\n  color: var(--dsw-alias-label-secondary);\n  cursor: pointer;\n  white-space: nowrap;\n  background: 0 0;\n  border: none;\n  border-radius: 8px;\n  align-items: center;\n  gap: 8px;\n  padding: 0 12px;\n  font-size: 13px;\n  display: flex;\n}\n\n.nyu-entry:hover {\n  background: var(--dsw-specific-sidebar-nav-item-hover);\n  color: var(--dsw-alias-label-primary);\n}\n\n.nyu-entry[data-active] {\n  background: var(--dsw-specific-sidebar-nav-item-active);\n  color: var(--dsw-alias-label-primary);\n  font-weight: 600;\n}\n\n.nyu-entryIcon {\n  flex: none;\n  justify-content: center;\n  align-items: center;\n  display: inline-flex;\n}\n\n.nyu-entryLabel {\n  text-overflow: ellipsis;\n  overflow: hidden;\n}\n\n[data-dsh-frame][data-sidebar-collapsed] .nyu-entry {\n  justify-content: center;\n  width: 100%;\n  padding: 0;\n}\n\n[data-dsh-frame][data-sidebar-collapsed] .nyu-entryLabel {\n  display: none;\n}\n\n/* --- panel frame ------------------------------------------------------------ */\n\n.nyu-panel {\n  background: var(--dsw-alias-bg-base);\n  min-width: 0;\n  height: 100%;\n  min-height: 0;\n  color: var(--dsw-alias-label-primary);\n  font-family: var(--dsw-font-family);\n  flex-direction: column;\n  gap: 12px;\n  padding: 14px 16px 16px;\n  display: flex;\n}\n\n.nyu-panelHeader {\n  flex: none;\n  align-items: center;\n  flex-wrap: wrap;\n  gap: 10px;\n  display: flex;\n}\n\n.nyu-panelTitle {\n  color: var(--dsw-alias-label-primary);\n  white-space: nowrap;\n  flex: none;\n  margin: 0;\n  font-size: 16px;\n  font-weight: 700;\n}\n\n.nyu-baseUrlChip {\n  color: var(--dsw-alias-label-tertiary);\n  background: var(--dsw-alias-bg-layer-2);\n  border: 1px solid var(--dsw-alias-border-l1);\n  white-space: nowrap;\n  max-width: 260px;\n  overflow: hidden;\n  text-overflow: ellipsis;\n  border-radius: 999px;\n  padding: 2px 10px;\n  font-size: 11.5px;\n  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;\n}\n\n.nyu-summary {\n  color: var(--dsw-alias-label-secondary);\n  white-space: nowrap;\n  font-size: 12px;\n}\n\n.nyu-toolbarSpacer {\n  flex: 1;\n}\n\n.nyu-autoRefreshLabel {\n  color: var(--dsw-alias-label-secondary);\n  align-items: center;\n  gap: 6px;\n  font-size: 12px;\n  display: inline-flex;\n}\n\n.nyu-autoRefreshSelect {\n  color: var(--dsw-alias-label-primary);\n  background: var(--dsw-specific-input-major);\n  border: 1px solid var(--dsw-alias-border-l2);\n  border-radius: 8px;\n  outline: none;\n  padding: 4px 8px;\n  font: inherit;\n  font-size: 12px;\n}\n\n.nyu-updated {\n  color: var(--dsw-alias-label-tertiary);\n  white-space: nowrap;\n  font-size: 11.5px;\n}\n\n.nyu-refreshButton {\n  color: var(--dsw-alias-label-primary-foreground);\n  background: var(--dsh-alias-button-info-fill, var(--dsw-alias-state-business-primary));\n  cursor: pointer;\n  white-space: nowrap;\n  align-items: center;\n  gap: 6px;\n  border: none;\n  border-radius: 8px;\n  padding: 6px 14px;\n  font-size: 13px;\n  font-weight: 600;\n  display: inline-flex;\n}\n\n.nyu-refreshButton:hover:not(:disabled) {\n  background: var(--dsh-alias-button-info-hover, var(--dsw-alias-state-business-primary));\n}\n\n.nyu-refreshButton:disabled {\n  opacity: 0.5;\n  cursor: default;\n}\n\n.nyu-spinner {\n  border: 2px solid currentcolor;\n  border-top-color: transparent;\n  border-radius: 50%;\n  flex: none;\n  width: 11px;\n  height: 11px;\n  animation: 0.8s linear infinite nyu-spin;\n  display: inline-block;\n}\n\n@keyframes nyu-spin {\n  to {\n    transform: rotate(360deg);\n  }\n}\n\n.nyu-panelContent {\n  flex-direction: column;\n  flex: 1;\n  gap: 12px;\n  min-height: 0;\n  display: flex;\n  overflow-y: auto;\n}\n\n.nyu-placeholder {\n  text-align: center;\n  color: var(--dsw-alias-label-tertiary);\n  gap: 6px;\n  padding: 40px 16px;\n  font-size: 12.5px;\n  display: flex;\n  flex-direction: column;\n}\n\n.nyu-placeholderTitle {\n  color: var(--dsw-alias-label-secondary);\n  margin: 0;\n  font-size: 14px;\n  font-weight: 600;\n}\n\n.nyu-placeholderHint {\n  margin: 0;\n  line-height: 1.6;\n}\n\n.nyu-loadError {\n  color: var(--dsw-alias-state-error-primary);\n  background: var(--dsw-alias-bg-layer-2);\n  border: 1px solid var(--dsw-alias-state-error-primary);\n  align-items: center;\n  justify-content: space-between;\n  gap: 12px;\n  border-radius: 10px;\n  padding: 10px 14px;\n  font-size: 12.5px;\n  line-height: 1.5;\n  display: flex;\n}\n\n.nyu-retryButton {\n  color: var(--dsw-alias-label-primary);\n  border: 1px solid var(--dsw-alias-border-l2);\n  cursor: pointer;\n  white-space: nowrap;\n  background: 0 0;\n  border-radius: 8px;\n  padding: 4px 12px;\n  font: inherit;\n  font-size: 12px;\n  flex: none;\n}\n\n.nyu-retryButton:hover {\n  background: var(--dsw-alias-interactive-bg-hover);\n}\n\n/* --- provider card grid ------------------------------------------------------ */\n\n.nyu-grid {\n  grid-template-columns: repeat(auto-fill, minmax(340px, 1fr));\n  gap: 12px;\n  display: grid;\n}\n\n.nyu-card {\n  background: var(--dsw-alias-bg-layer-2);\n  border: 1px solid var(--dsw-alias-border-l1);\n  border-radius: 12px;\n  flex-direction: column;\n  gap: 10px;\n  padding: 12px 14px;\n  display: flex;\n  min-width: 0;\n}\n\n/* --- drag-to-reorder --------------------------------------------------------- */\n\n/* Grip affordance: the whole card is draggable, the grip just says so. */\n.nyu-dragHandle {\n  color: var(--dsw-alias-label-tertiary);\n  cursor: grab;\n  flex: none;\n  font-size: 14px;\n  line-height: 1;\n  opacity: 0.8;\n}\n\n.nyu-dragHandle:hover {\n  color: var(--dsw-alias-label-secondary);\n  opacity: 1;\n}\n\n/* Source card dims while a drag is in flight. */\n.nyu-cardDragging {\n  opacity: 0.4;\n}\n\n/* Insertion indicator: a business-colored bar on the target edge. */\n.nyu-cardDropBefore {\n  box-shadow: -3px 0 0 0 var(--dsw-alias-state-business-primary);\n}\n\n.nyu-cardDropAfter {\n  box-shadow: 3px 0 0 0 var(--dsw-alias-state-business-primary);\n}\n\n.nyu-resetOrderButton {\n  color: var(--dsw-alias-label-secondary);\n  background: 0 0;\n  border: 1px solid var(--dsw-alias-border-l2);\n  cursor: pointer;\n  white-space: nowrap;\n  border-radius: 8px;\n  padding: 5px 12px;\n  font: inherit;\n  font-size: 12px;\n  flex: none;\n}\n\n.nyu-resetOrderButton:hover {\n  color: var(--dsw-alias-label-primary);\n  background: var(--dsw-alias-interactive-bg-hover);\n}\n\n.nyu-cardMuted {\n  opacity: 0.55;\n}\n\n.nyu-cardExhausted {\n  border-color: var(--dsw-alias-state-error-primary);\n}\n\n.nyu-cardHeader {\n  align-items: center;\n  gap: 8px;\n  display: flex;\n}\n\n.nyu-cardTitle {\n  color: var(--dsw-alias-label-primary);\n  text-overflow: ellipsis;\n  white-space: nowrap;\n  min-width: 0;\n  flex: 1;\n  overflow: hidden;\n  font-size: 13.5px;\n  font-weight: 600;\n}\n\n.nyu-cardBadges {\n  align-items: center;\n  gap: 6px;\n  flex-wrap: wrap;\n  justify-content: flex-end;\n  display: inline-flex;\n}\n\n.nyu-badge {\n  color: var(--dsw-alias-label-secondary);\n  background: var(--dsw-alias-bg-layer-3);\n  border: 1px solid var(--dsw-alias-border-l2);\n  white-space: nowrap;\n  border-radius: 999px;\n  padding: 1px 8px;\n  font-size: 11px;\n  line-height: 1.6;\n  display: inline-block;\n}\n\n.nyu-badgeLevel {\n  color: var(--dsw-alias-state-business-primary);\n  border-color: var(--dsw-alias-state-business-primary);\n  background: 0 0;\n}\n\n.nyu-pill {\n  white-space: nowrap;\n  border-radius: 999px;\n  padding: 1px 8px;\n  font-size: 11px;\n  font-weight: 600;\n  line-height: 1.6;\n  display: inline-block;\n}\n\n.nyu-pillEligible {\n  color: var(--dsw-alias-state-success-primary);\n  border: 1px solid var(--dsw-alias-state-success-primary);\n}\n\n.nyu-pillExhausted {\n  color: #fff;\n  background: var(--dsw-alias-state-error-primary);\n  border: 1px solid var(--dsw-alias-state-error-primary);\n}\n\n.nyu-errorBanner {\n  color: var(--dsw-alias-state-error-primary);\n  background: var(--dsw-alias-bg-layer-3);\n  border: 1px solid var(--dsw-alias-separator-primary);\n  border-left: 3px solid var(--dsw-alias-state-error-primary);\n  flex-direction: column;\n  gap: 2px;\n  border-radius: 8px;\n  padding: 8px 10px;\n  display: flex;\n}\n\n.nyu-errorTitle {\n  font-size: 11.5px;\n  font-weight: 600;\n}\n\n.nyu-errorText {\n  color: var(--dsw-alias-label-secondary);\n  overflow-wrap: anywhere;\n  font-size: 11.5px;\n  line-height: 1.5;\n}\n\n/* --- quota tier rows --------------------------------------------------------- */\n\n.nyu-tierList {\n  flex-direction: column;\n  gap: 7px;\n  display: flex;\n}\n\n.nyu-tierRow {\n  align-items: center;\n  gap: 8px;\n  font-size: 12px;\n  display: flex;\n}\n\n.nyu-tierLabel {\n  color: var(--dsw-alias-label-secondary);\n  text-overflow: ellipsis;\n  white-space: nowrap;\n  min-width: 84px;\n  max-width: 150px;\n  overflow: hidden;\n  flex: none;\n  font-weight: 500;\n}\n\n.nyu-tierTrack {\n  background: var(--dsw-alias-interactive-bg-hover);\n  border-radius: 999px;\n  height: 8px;\n  flex: 1;\n  /* visible overflow: the steady-pace triangle renders above the track. */\n  overflow: visible;\n  position: relative;\n}\n\n.nyu-tierFill {\n  border-radius: 999px;\n  height: 100%;\n  transition: width 0.15s linear;\n  /* clip the fill itself to the track's rounded shape. */\n  overflow: hidden;\n}\n\n/* Steady-pace indicator: a small triangle hovering above the track,\n   pointing down at the position an even consumer would be at right now\n   (0% right after reset → 100% at reset time). Rendered outside the track's\n   rounded clipping via the wrapper's overflow: visible. */\n.nyu-tierPaceWrap {\n  position: absolute;\n  top: -8px;\n  left: 0;\n  right: 0;\n  height: 0;\n  pointer-events: none;\n}\n\n.nyu-tierPace {\n  position: absolute;\n  top: 0;\n  width: 0;\n  height: 0;\n  transform: translateX(-50%);\n  border-left: 4px solid transparent;\n  border-right: 4px solid transparent;\n  /* Theme-tracking ink: the token flips with body[data-ds-dark-theme] —\n     bright (neutral-50) on dark backgrounds, dark (neutral-1000) on light\n     ones. The bg-colored drop-shadow halo separates the shape from the\n     colored fill and the pale track in both themes. */\n  border-top: 5px solid var(--dsw-alias-label-primary, #334155);\n  filter: drop-shadow(0 0 1.5px var(--dsw-alias-bg-base, rgba(255, 255, 255, 0.9)));\n  cursor: help;\n  pointer-events: auto;\n}\n\n.nyu-tierFillOk {\n  background: var(--dsw-alias-state-success-primary);\n}\n\n.nyu-tierFillWarn {\n  background: var(--dsw-alias-state-warn-primary);\n}\n\n.nyu-tierFillDanger {\n  background: var(--dsw-alias-state-error-primary);\n}\n\n/* --- balances / spends ------------------------------------------------------- */\n\n.nyu-tierPercent {\n  width: 38px;\n  text-align: right;\n  flex: none;\n  font-weight: 600;\n  font-variant-numeric: tabular-nums;\n}\n\n.nyu-tierPercentOk {\n  color: var(--dsw-alias-state-success-primary);\n}\n\n.nyu-tierPercentWarn {\n  color: var(--dsw-alias-state-warn-primary);\n}\n\n.nyu-tierPercentDanger {\n  color: var(--dsw-alias-state-error-primary);\n}\n\n.nyu-tierReset {\n  color: var(--dsw-alias-label-tertiary);\n  white-space: nowrap;\n  width: 52px;\n  text-align: right;\n  flex: none;\n  font-size: 10.5px;\n  font-variant-numeric: tabular-nums;\n}\n\n/* --- balances / spends ------------------------------------------------------- */\n\n.nyu-balanceList {\n  flex-direction: column;\n  gap: 5px;\n  display: flex;\n}\n\n.nyu-balanceRow {\n  align-items: baseline;\n  gap: 6px;\n  font-size: 12px;\n  display: flex;\n}\n\n.nyu-balanceLabel {\n  color: var(--dsw-alias-label-secondary);\n  width: 52px;\n  flex: none;\n  font-weight: 500;\n}\n\n.nyu-balanceValue {\n  color: var(--dsw-alias-label-primary);\n  font-weight: 600;\n  font-variant-numeric: tabular-nums;\n}\n\n.nyu-balanceDetail {\n  color: var(--dsw-alias-label-tertiary);\n  font-size: 10.5px;\n}\n\n.nyu-unavailableBadge {\n  color: var(--dsw-alias-state-error-primary);\n  font-weight: 700;\n}\n\n.nyu-spendRow {\n  color: var(--dsw-alias-label-secondary);\n  align-items: center;\n  flex-wrap: wrap;\n  gap: 4px 14px;\n  font-size: 12px;\n  display: flex;\n}\n\n.nyu-spendValue {\n  color: var(--dsw-alias-label-primary);\n  font-weight: 600;\n  font-variant-numeric: tabular-nums;\n}\n\n.nyu-cardFooter {\n  color: var(--dsw-alias-label-tertiary);\n  font-size: 10.5px;\n}\n\n@media (prefers-reduced-motion: reduce) {\n  .nyu-tierFill,\n  .nyu-spinner {\n    transition: none;\n    animation-duration: 1.6s;\n  }\n}\n";
if (typeof document !== "undefined") {
	const tag = "data-dsh-nyro-usage-css";
	if (document.querySelector("style[" + tag + "]") === null) {
		const el = document.createElement("style");
		el.setAttribute(tag, "");
		el.textContent = cssText$1;
		document.head.appendChild(el);
	}
}
var panel_module_css_default = {
	"entry": "nyu-entry",
	"entryIcon": "nyu-entryIcon",
	"entryLabel": "nyu-entryLabel",
	"panel": "nyu-panel",
	"panelHeader": "nyu-panelHeader",
	"panelTitle": "nyu-panelTitle",
	"baseUrlChip": "nyu-baseUrlChip",
	"summary": "nyu-summary",
	"toolbarSpacer": "nyu-toolbarSpacer",
	"autoRefreshLabel": "nyu-autoRefreshLabel",
	"autoRefreshSelect": "nyu-autoRefreshSelect",
	"updated": "nyu-updated",
	"refreshButton": "nyu-refreshButton",
	"spinner": "nyu-spinner",
	"panelContent": "nyu-panelContent",
	"placeholder": "nyu-placeholder",
	"placeholderTitle": "nyu-placeholderTitle",
	"placeholderHint": "nyu-placeholderHint",
	"loadError": "nyu-loadError",
	"retryButton": "nyu-retryButton",
	"grid": "nyu-grid",
	"card": "nyu-card",
	"dragHandle": "nyu-dragHandle",
	"cardDragging": "nyu-cardDragging",
	"cardDropBefore": "nyu-cardDropBefore",
	"cardDropAfter": "nyu-cardDropAfter",
	"resetOrderButton": "nyu-resetOrderButton",
	"cardMuted": "nyu-cardMuted",
	"cardExhausted": "nyu-cardExhausted",
	"cardHeader": "nyu-cardHeader",
	"cardTitle": "nyu-cardTitle",
	"cardBadges": "nyu-cardBadges",
	"badge": "nyu-badge",
	"badgeLevel": "nyu-badgeLevel",
	"pill": "nyu-pill",
	"pillEligible": "nyu-pillEligible",
	"pillExhausted": "nyu-pillExhausted",
	"errorBanner": "nyu-errorBanner",
	"errorTitle": "nyu-errorTitle",
	"errorText": "nyu-errorText",
	"tierList": "nyu-tierList",
	"tierRow": "nyu-tierRow",
	"tierLabel": "nyu-tierLabel",
	"tierTrack": "nyu-tierTrack",
	"tierFill": "nyu-tierFill",
	"tierPaceWrap": "nyu-tierPaceWrap",
	"tierPace": "nyu-tierPace",
	"tierFillOk": "nyu-tierFillOk",
	"tierFillWarn": "nyu-tierFillWarn",
	"tierFillDanger": "nyu-tierFillDanger",
	"tierPercent": "nyu-tierPercent",
	"tierPercentOk": "nyu-tierPercentOk",
	"tierPercentWarn": "nyu-tierPercentWarn",
	"tierPercentDanger": "nyu-tierPercentDanger",
	"tierReset": "nyu-tierReset",
	"balanceList": "nyu-balanceList",
	"balanceRow": "nyu-balanceRow",
	"balanceLabel": "nyu-balanceLabel",
	"balanceValue": "nyu-balanceValue",
	"balanceDetail": "nyu-balanceDetail",
	"unavailableBadge": "nyu-unavailableBadge",
	"spendRow": "nyu-spendRow",
	"spendValue": "nyu-spendValue",
	"cardFooter": "nyu-cardFooter"
};
//#endregion
//#region src/client/panel/TierBar.tsx
/** Pretty-print a tier name: known windows localized, `feature:<f>:<w>`
* rendered as "<Feature> · <window>", anything else humanized. */
function tierLabel(name) {
	const known = [
		"five_hour",
		"weekly_limit",
		"monthly",
		"primary_window",
		"secondary_window"
	];
	const feature = /^feature:(.+):(five_hour|weekly_limit|monthly|primary_window|secondary_window)$/.exec(name);
	if (feature !== null) return tt("tier.feature", {
		feature: feature[1].replace(/[_-]+/g, " ").replace(/\s+/g, " ").trim().replace(/\b\w/g, (character) => character.toUpperCase()),
		window: tt(`tier.${feature[2]}`)
	});
	if (known.includes(name)) return tt(`tier.${name}`);
	return name.replace(/[_-]+/g, " ").replace(/\b\w/g, (character) => character.toUpperCase());
}
/** Utilization → progress-bar color state. */
function tierState(used) {
	if (used >= 90) return "danger";
	if (used >= 70) return "warn";
	return "ok";
}
/** Window length per tier name, used to place the steady-pace marker
* (mirrors the nyro webui coding-plan footer). */
function tierWindowMs(name) {
	if (name.startsWith("feature:")) return null;
	switch (name) {
		case "five_hour": return 18e6;
		case "weekly_limit": return 6048e5;
		case "monthly": return 2592e6;
		default: return null;
	}
}
/**
* Steady-pace position (0-100): where a perfectly even consumer would sit
* right now. Computed from the reset time walking the window backwards:
* `elapsed / window` — 0 right after reset, 100 at reset time. Null when the
* tier carries no reset time or an unknown window.
*/
function steadyPacePercent(resetsAt, name, now) {
	if (resetsAt === null || resetsAt === void 0) return null;
	const windowMs = tierWindowMs(name);
	if (windowMs === null) return null;
	const remaining = new Date(resetsAt).getTime() - now;
	if (!Number.isFinite(remaining)) return null;
	if (remaining <= 0) return 100;
	if (remaining >= windowMs) return 0;
	return (windowMs - remaining) / windowMs * 100;
}
/** Render one quota-window row. */
function TierBar(props) {
	const used = Math.min(Math.max(Number.isFinite(props.tier.used_percent) ? props.tier.used_percent : 0, 0), 100);
	const countdown = countdownLabel(props.tier.resets_at);
	const state = tierState(used);
	const pace = steadyPacePercent(props.tier.resets_at, props.tier.name, props.now);
	return /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
		className: panel_module_css_default.tierRow,
		title: props.tier.name,
		children: [
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
				className: panel_module_css_default.tierLabel,
				children: tierLabel(props.tier.name)
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
				className: panel_module_css_default.tierTrack,
				children: [pace !== null ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
					className: panel_module_css_default.tierPaceWrap,
					children: /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
						className: panel_module_css_default.tierPace,
						style: { left: `${Math.min(Math.max(pace, 0), 100)}%` },
						title: tt("tier.pace", { percent: Math.round(pace) })
					})
				}) : null, /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
					className: `${panel_module_css_default.tierFill} ${state === "ok" ? panel_module_css_default.tierFillOk : state === "warn" ? panel_module_css_default.tierFillWarn : panel_module_css_default.tierFillDanger}`,
					style: { width: `${used}%` }
				})]
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
				className: `${panel_module_css_default.tierPercent} ${state === "ok" ? panel_module_css_default.tierPercentOk : state === "warn" ? panel_module_css_default.tierPercentWarn : panel_module_css_default.tierPercentDanger}`,
				children: [Math.round(used), "%"]
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
				className: panel_module_css_default.tierReset,
				title: props.tier.resets_at ?? void 0,
				children: countdown === "" ? "" : `⏳ ${countdown}`
			})
		]
	});
}
//#endregion
//#region src/client/panel/ProviderCard.tsx
/** Currency → symbol (anything else keeps its ISO code). */
function currencySymbol(currency) {
	if (currency === "CNY") return "¥";
	if (currency === "USD") return "$";
	return `${currency} `;
}
/** Render one provider card. */
function ProviderCard(props) {
	const { item } = props;
	const usage = item.usage;
	const exhausted = usage?.scheduling?.status === "quota_exhausted";
	const balances = usage?.balances ?? [];
	const spends = usage?.spends ?? [];
	const tiers = usage?.tiers ?? [];
	const classes = [panel_module_css_default.card];
	if (!item.is_enabled) classes.push(panel_module_css_default.cardMuted);
	if (exhausted) classes.push(panel_module_css_default.cardExhausted);
	if (props.dragging === true) classes.push(panel_module_css_default.cardDragging);
	if (props.dropBefore === true) classes.push(panel_module_css_default.cardDropBefore);
	if (props.dropAfter === true) classes.push(panel_module_css_default.cardDropAfter);
	return /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
		className: classes.join(" "),
		draggable: true,
		onDragStart: props.onCardDragStart,
		onDragEnd: props.onCardDragEnd,
		onDragOver: props.onCardDragOver,
		onDrop: props.onCardDrop,
		children: [
			/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
				className: panel_module_css_default.cardHeader,
				children: [
					/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
						className: panel_module_css_default.dragHandle,
						title: tt("card.dragHandle"),
						"aria-hidden": "true",
						children: "⠿"
					}),
					/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
						className: panel_module_css_default.cardTitle,
						title: item.provider_id,
						children: item.provider_name
					}),
					/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
						className: panel_module_css_default.cardBadges,
						children: [
							usage !== null && usage !== void 0 && usage.kind !== "" ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
								className: panel_module_css_default.badge,
								children: usage.kind
							}) : null,
							usage?.level != null && usage.level !== "" ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
								className: `${panel_module_css_default.badge} ${panel_module_css_default.badgeLevel}`,
								children: usage.level
							}) : null,
							!item.is_enabled ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
								className: panel_module_css_default.badge,
								children: tt("card.disabled")
							}) : null,
							exhausted ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
								className: `${panel_module_css_default.pill} ${panel_module_css_default.pillExhausted}`,
								children: tt("card.quotaExhausted")
							}) : /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
								className: `${panel_module_css_default.pill} ${panel_module_css_default.pillEligible}`,
								children: tt("card.eligible")
							})
						]
					})
				]
			}),
			item.status === "error" ? /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
				className: panel_module_css_default.errorBanner,
				role: "alert",
				children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: panel_module_css_default.errorTitle,
					children: tt("card.error")
				}), /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: panel_module_css_default.errorText,
					children: item.error ?? ""
				})]
			}) : null,
			tiers.length > 0 ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
				className: panel_module_css_default.tierList,
				children: tiers.map((tier) => /* @__PURE__ */ (0, react_jsx_runtime.jsx)(TierBar, {
					tier,
					now: props.now
				}, tier.name))
			}) : null,
			balances.length > 0 ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
				className: panel_module_css_default.balanceList,
				children: balances.map((balance) => /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
					className: panel_module_css_default.balanceRow,
					children: [
						/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
							className: panel_module_css_default.balanceLabel,
							children: tt("card.balance")
						}),
						/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
							className: panel_module_css_default.balanceValue,
							children: [currencySymbol(balance.currency), balance.total.toFixed(2)]
						}),
						/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
							className: panel_module_css_default.balanceDetail,
							children: balance.granted > 0 ? ` (+${balance.topped_up.toFixed(2)} / ${balance.granted.toFixed(2)})` : ""
						}),
						usage?.is_available === false ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
							className: panel_module_css_default.unavailableBadge,
							children: "✗"
						}) : null
					]
				}, balance.currency))
			}) : null,
			spends.length > 0 ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
				className: panel_module_css_default.spendRow,
				children: spends.map((spend) => /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
					className: panel_module_css_default.spendItem,
					title: spend.name,
					children: [
						spend.name === "today" ? tt("card.spendToday") : spend.name === "month" ? tt("card.spendMonth") : spend.name,
						" ",
						/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
							className: panel_module_css_default.spendValue,
							children: [currencySymbol(spend.currency), spend.amount.toFixed(2)]
						})
					]
				}, spend.name))
			}) : null,
			usage?.queried_at != null && usage.queried_at !== "" ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
				className: panel_module_css_default.cardFooter,
				children: tt("card.queriedAt", { ago: agoLabel(usage.queried_at) })
			}) : null
		]
	});
}
//#endregion
//#region src/client/panel/NyroUsagePanel.tsx
/**
* The nyro usage panel: header (base URL chip, scheduling summary, manual
* refresh, auto-refresh selector, last-updated stamp) over a responsive
* grid of provider cards the user can drag to reorder (order persisted per
* browser). Loads through the same-origin host proxy; state machine:
* unconfigured → loading → data | error.
*/
/** Auto-refresh choices offered next to the configured default. */
const AUTO_REFRESH_CHOICES = [
	0,
	60,
	300,
	600
];
/** Render the nyro provider usage panel. */
function NyroUsagePanel(props) {
	const { api } = props;
	const snapshot = (0, react.useSyncExternalStore)(props.controller.subscribe, props.controller.getSnapshot);
	const [status, setStatus] = (0, react.useState)(null);
	const [state, setState] = (0, react.useState)({
		loading: false,
		items: [],
		cachedAt: null,
		error: null,
		errorKind: null
	});
	const [autoSeconds, setAutoSeconds] = (0, react.useState)(null);
	const { order, move, append, reset } = useCardOrder();
	const [dragId, setDragId] = (0, react.useState)(null);
	const [dropSide, setDropSide] = (0, react.useState)(null);
	const [now, setNow] = (0, react.useState)(() => Date.now());
	const inFlight = (0, react.useRef)(false);
	const loadSeq = (0, react.useRef)(0);
	(0, react.useEffect)(() => {
		const timer = window.setInterval(() => {
			setNow(Date.now());
		}, 3e4);
		return () => {
			window.clearInterval(timer);
		};
	}, []);
	const load = (0, react.useCallback)(async (refresh) => {
		if (inFlight.current) return;
		inFlight.current = true;
		const seq = ++loadSeq.current;
		setState((previous) => ({
			...previous,
			loading: true
		}));
		try {
			const [nextStatus, usage] = await Promise.all([api.status().catch(() => null), api.usage(refresh)]);
			if (seq !== loadSeq.current) return;
			if (nextStatus !== null) setStatus(nextStatus);
			setState({
				loading: false,
				items: usage.data,
				cachedAt: usage.cachedAt,
				error: null,
				errorKind: null
			});
		} catch (error) {
			if (seq !== loadSeq.current) return;
			const message = errorMessage(error);
			const kind = error instanceof NyroPanelApiError ? error.kind ?? null : null;
			setState((previous) => ({
				...previous,
				loading: false,
				error: message,
				errorKind: kind
			}));
		} finally {
			inFlight.current = false;
		}
	}, [api]);
	(0, react.useEffect)(() => {
		if (!snapshot.panelOpen) return;
		api.status().then((next) => {
			setStatus(next);
		}).catch(() => {});
	}, [api, snapshot.panelOpen]);
	const configured = status?.configured ?? false;
	(0, react.useEffect)(() => {
		if (!snapshot.panelOpen || !configured) return;
		load(false);
	}, [
		snapshot.panelOpen,
		configured,
		load
	]);
	const interval = autoSeconds ?? status?.refreshSeconds ?? 300;
	(0, react.useEffect)(() => {
		if (!snapshot.panelOpen || !configured || interval <= 0) return;
		const timer = window.setInterval(() => {
			load(false);
		}, interval * 1e3);
		return () => {
			window.clearInterval(timer);
		};
	}, [
		snapshot.panelOpen,
		configured,
		interval,
		load
	]);
	const exhausted = state.items.filter((item) => item.usage?.scheduling?.status === "quota_exhausted").length;
	const ok = state.items.filter((item) => item.status === "ok").length;
	const visible = bySavedOrder(state.items.filter((item) => item.status !== "unsupported"), order, (item) => item.provider_id);
	const visibleIds = visible.map((item) => item.provider_id);
	const updated = agoLabel(state.cachedAt);
	const clearDrag = (0, react.useCallback)(() => {
		setDragId(null);
		setDropSide(null);
	}, []);
	const sideOf = (event) => {
		const rect = event.currentTarget.getBoundingClientRect();
		return event.clientX < rect.left + rect.width / 2;
	};
	const cardDragHandlers = (item) => ({
		onCardDragStart: (event) => {
			setDragId(item.provider_id);
			event.dataTransfer.effectAllowed = "move";
			event.dataTransfer.setData("text/plain", item.provider_id);
		},
		onCardDragEnd: clearDrag,
		onCardDragOver: (event) => {
			if (dragId === null) return;
			event.preventDefault();
			event.dataTransfer.dropEffect = "move";
			if (dragId === item.provider_id) return;
			const before = sideOf(event);
			setDropSide((previous) => previous !== null && previous.id === item.provider_id && previous.before === before ? previous : {
				id: item.provider_id,
				before
			});
		},
		onCardDrop: (event) => {
			if (dragId === null) return;
			event.preventDefault();
			event.stopPropagation();
			if (dragId !== item.provider_id) move(visibleIds, dragId, item.provider_id, sideOf(event));
			clearDrag();
		}
	});
	return /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
		className: panel_module_css_default.panel,
		children: [/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
			className: panel_module_css_default.panelHeader,
			children: [
				/* @__PURE__ */ (0, react_jsx_runtime.jsx)("h1", {
					className: panel_module_css_default.panelTitle,
					children: tt("panel.title")
				}),
				status !== null && status.baseUrl !== "" ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: panel_module_css_default.baseUrlChip,
					title: status.baseUrl,
					children: status.baseUrl
				}) : null,
				state.items.length > 0 ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: panel_module_css_default.summary,
					children: tt("panel.summary", {
						ok,
						exhausted
					})
				}) : null,
				/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", { className: panel_module_css_default.toolbarSpacer }),
				order.length > 0 ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("button", {
					type: "button",
					className: panel_module_css_default.resetOrderButton,
					onClick: reset,
					children: tt("panel.resetOrder")
				}) : null,
				/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("label", {
					className: panel_module_css_default.autoRefreshLabel,
					children: [tt("panel.autoRefresh"), /* @__PURE__ */ (0, react_jsx_runtime.jsx)("select", {
						className: panel_module_css_default.autoRefreshSelect,
						value: String(interval),
						onChange: (event) => {
							setAutoSeconds(Number(event.target.value));
						},
						children: AUTO_REFRESH_CHOICES.map((choice) => /* @__PURE__ */ (0, react_jsx_runtime.jsx)("option", {
							value: String(choice),
							children: choice === 0 ? tt("panel.autoRefresh.off") : choice >= 60 ? `${choice / 60}m` : `${choice}s`
						}, choice))
					})]
				}),
				/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: panel_module_css_default.updated,
					title: state.cachedAt ?? void 0,
					children: updated === "" ? "" : tt("panel.updated", { ago: updated })
				}),
				/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("button", {
					type: "button",
					className: panel_module_css_default.refreshButton,
					disabled: state.loading || !configured,
					onClick: () => {
						load(true);
					},
					children: [state.loading ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
						className: panel_module_css_default.spinner,
						"aria-hidden": "true"
					}) : null, state.loading ? tt("panel.refreshing") : tt("panel.refresh")]
				})
			]
		}), /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
			className: panel_module_css_default.panelContent,
			children: [
				status !== null && !configured ? /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
					className: panel_module_css_default.placeholder,
					children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("p", {
						className: panel_module_css_default.placeholderTitle,
						children: tt("panel.notConfigured")
					}), /* @__PURE__ */ (0, react_jsx_runtime.jsx)("p", {
						className: panel_module_css_default.placeholderHint,
						children: tt("panel.notConfiguredHint")
					})]
				}) : null,
				configured && state.error !== null ? /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
					className: panel_module_css_default.loadError,
					role: "alert",
					children: [/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", { children: [
						tt("panel.loadError"),
						"：",
						state.error
					] }), /* @__PURE__ */ (0, react_jsx_runtime.jsx)("button", {
						type: "button",
						className: panel_module_css_default.retryButton,
						onClick: () => {
							load(true);
						},
						children: tt("panel.retry")
					})]
				}) : null,
				configured && state.error === null && !state.loading && visible.length === 0 ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
					className: panel_module_css_default.placeholder,
					children: /* @__PURE__ */ (0, react_jsx_runtime.jsx)("p", {
						className: panel_module_css_default.placeholderHint,
						children: tt("panel.empty")
					})
				}) : null,
				visible.length > 0 ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
					className: panel_module_css_default.grid,
					onDragOver: (event) => {
						if (dragId === null) return;
						event.preventDefault();
						event.dataTransfer.dropEffect = "move";
					},
					onDrop: (event) => {
						if (dragId === null) return;
						event.preventDefault();
						append(visibleIds, dragId);
						clearDrag();
					},
					children: visible.map((item) => /* @__PURE__ */ (0, react_jsx_runtime.jsx)(ProviderCard, {
						item,
						now,
						dragging: dragId === item.provider_id,
						dropBefore: dropSide !== null && dropSide.id === item.provider_id && dropSide.before && dragId !== item.provider_id,
						dropAfter: dropSide !== null && dropSide.id === item.provider_id && !dropSide.before && dragId !== item.provider_id,
						...cardDragHandlers(item)
					}, item.provider_id))
				}) : null,
				configured && state.loading && state.items.length === 0 && state.error === null ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
					className: panel_module_css_default.placeholder,
					children: /* @__PURE__ */ (0, react_jsx_runtime.jsx)("p", {
						className: panel_module_css_default.placeholderHint,
						children: tt("panel.refreshing")
					})
				}) : null
			]
		})]
	});
}
//#endregion
//#region src/client/mount.tsx
/**
* Panel view mounting.
*
* The `conversation` slot is single-occupant (ui-conversation) and external
* plugins cannot declare slots, so the panel takes over the center column at
* the DOM level (task-board / dsh-ssh precedent): a container is appended
* inside the center column as an extra trailing child React never manages,
* and a stylesheet rule hides the conversation content while the panel is
* active. Toggling is a data attribute on <html> — no React involvement, so
* the conversation subtree underneath stays mounted and stateful.
*/
const CONVERSATION_COLUMN_SELECTOR = "[data-pane=\"conversation\"], [class*=\"centerCol\"]";
const ACTIVE_ATTR = "data-dsh-nyro-usage-active";
/** Sibling panels' activation attributes, removed when this panel opens. */
const OTHER_ACTIVE_ATTRS = ["data-dsh-taskboard-active", "data-dsh-ssh-active"];
/** Cross-plugin activation event; detail is the activating panel name. */
const ACTIVATE_EVENT = "dsh-panel-activate";
const PANEL_NAME = "nyro-usage";
/** Find the center column, or undefined while the frame is not mounted. */
function conversationColumn() {
	return document.querySelector(CONVERSATION_COLUMN_SELECTOR) ?? void 0;
}
/**
* Mount the panel React tree into the center column and bind its visibility
* to the controller's panelOpen state.
* @param controller - the panel controller driving the view.
* @param api - the host proxy API client the panel operates through.
* @returns disposer unmounting the tree and restoring the column.
*/
function mountPanel(controller, api) {
	let root;
	let container;
	const ensure = () => {
		if (container !== void 0) {
			if (container.isConnected) return;
			root?.unmount();
			root = void 0;
			container.remove();
			container = void 0;
		}
		const column = conversationColumn();
		if (column === void 0) return;
		container = document.createElement("div");
		container.dataset.dshNyroUsageView = "";
		container.className = panel_module_css_default.view;
		column.appendChild(container);
		root = (0, react_dom_client.createRoot)(container);
		root.render(/* @__PURE__ */ (0, react_jsx_runtime.jsx)(NyroUsagePanel, {
			controller,
			api
		}));
	};
	const waitObserver = new MutationObserver(() => {
		ensure();
	});
	waitObserver.observe(document.body, {
		childList: true,
		subtree: true
	});
	const applyActive = () => {
		if (controller.getSnapshot().panelOpen) {
			for (const attr of OTHER_ACTIVE_ATTRS) document.documentElement.removeAttribute(attr);
			document.documentElement.setAttribute(ACTIVE_ATTR, "");
			document.dispatchEvent(new CustomEvent(ACTIVATE_EVENT, { detail: PANEL_NAME }));
		} else document.documentElement.removeAttribute(ACTIVE_ATTR);
	};
	const onOtherActivate = (event) => {
		const detail = event.detail;
		if (typeof detail === "string" && detail !== PANEL_NAME && controller.getSnapshot().panelOpen) controller.close();
	};
	const SIDEBAR_ROW_SELECTOR = "[class*=\"sessionRow\"], [class*=\"projectRow\"], [class*=\"searchResultRow\"], [class*=\"searchResultWorkspace\"], [class*=\"newSession\"]";
	const onClickSidebarRow = (event) => {
		if (!controller.getSnapshot().panelOpen) return;
		const target = event.target;
		if (target === null) return;
		if (target.closest(SIDEBAR_ROW_SELECTOR) !== null) controller.close();
	};
	document.addEventListener("click", onClickSidebarRow, true);
	document.addEventListener(ACTIVATE_EVENT, onOtherActivate);
	const unsubscribe = controller.subscribe(applyActive);
	applyActive();
	ensure();
	return () => {
		document.removeEventListener("click", onClickSidebarRow, true);
		document.removeEventListener(ACTIVATE_EVENT, onOtherActivate);
		waitObserver.disconnect();
		unsubscribe();
		document.documentElement.removeAttribute(ACTIVE_ATTR);
		root?.unmount();
		root = void 0;
		container?.remove();
		container = void 0;
	};
}
//#endregion
//#region \0dsh-nyro-usage-css:/home/ubuntu/code/dsh-nyro-usage/src/client/settings-card.module.css.js
const cssText = "/* Generated by scripts/sync-shared.mjs from shared/client/settings/settings-card.module.css. Do not edit this copy; edit the shared source and run \"node scripts/sync-shared.mjs\". */\n/* Plugin settings card chrome + staged form fields.\n * Aligned with the official ui-settings-plugins PluginCard / fields CSS:\n * same semantic tokens, radius, typography and states so family cards read\n * as siblings of the built-in Shell / Agent loop / Web search cards. */\n\n.nyu-card {\n  border: 1px solid var(--dsw-alias-border-l2);\n  background: var(--dsw-alias-bg-layer-3);\n  border-radius: 12px;\n  list-style: none;\n  transition: border-color 0.16s, background 0.16s;\n}\n\n.nyu-card:hover {\n  border-color: var(--dsw-alias-label-dimmed);\n}\n\n.nyu-cardOpen {\n  background: var(--dsw-alias-bg-layer-2);\n  border-color: var(--dsw-alias-label-dimmed);\n}\n\n.nyu-header {\n  appearance: none;\n  width: 100%;\n  font: inherit;\n  color: inherit;\n  text-align: left;\n  cursor: pointer;\n  background: transparent;\n  border: 0;\n  border-radius: 12px;\n  align-items: center;\n  gap: 12px;\n  padding: 14px 16px;\n  display: flex;\n}\n\n.nyu-header:focus-visible {\n  outline: 2px solid var(--dsw-alias-brand-primary);\n  outline-offset: -2px;\n}\n\n.nyu-headerStatic {\n  width: 100%;\n  border-radius: 12px;\n  align-items: center;\n  gap: 12px;\n  padding: 14px 16px;\n  display: flex;\n}\n\n.nyu-headText {\n  flex-direction: column;\n  flex: 1;\n  gap: 4px;\n  min-width: 0;\n  display: flex;\n}\n\n.nyu-name {\n  color: var(--dsw-alias-label-primary);\n  font-size: 15px;\n  font-weight: 600;\n  line-height: 1.4;\n}\n\n.nyu-description {\n  color: var(--dsw-alias-label-tertiary);\n  font-size: 13px;\n  line-height: 1.5;\n}\n\n.nyu-pending {\n  white-space: nowrap;\n  background: var(--dsw-alias-bg-module-platform);\n  color: var(--dsw-alias-label-secondary);\n  border-radius: 999px;\n  flex: none;\n  padding: 1px 8px;\n  font-size: 11px;\n  font-weight: 500;\n  line-height: 17px;\n}\n\n.nyu-chevron {\n  color: var(--dsw-alias-label-tertiary);\n  flex: none;\n  transition: transform 0.16s;\n}\n\n.nyu-chevronOpen {\n  transform: rotate(180deg);\n}\n\n.nyu-body {\n  border-top: 1px solid var(--dsw-alias-border-l2);\n  margin: 0 16px;\n  padding-bottom: 8px;\n}\n\n.nyu-readOnly {\n  color: var(--dsw-alias-label-tertiary);\n  margin: 12px 0 0;\n  font-size: 12px;\n  line-height: 1.5;\n}\n\n.nyu-notExposed {\n  color: var(--dsw-alias-state-warn-primary);\n  margin: 12px 0 0;\n  font-size: 12px;\n  line-height: 1.5;\n}\n\n.nyu-footer {\n  border-top: 1px solid var(--dsw-alias-border-l2);\n  justify-content: flex-end;\n  align-items: center;\n  gap: 8px;\n  padding: 12px 0 4px;\n  display: flex;\n}\n\n.nyu-failed {\n  min-width: 0;\n  color: var(--dsw-alias-label-error);\n  flex: 1;\n  margin: 0;\n  font-size: 12px;\n  line-height: 1.5;\n  text-overflow: ellipsis;\n  overflow: hidden;\n  white-space: nowrap;\n}\n\n.nyu-discard,\n.nyu-save {\n  appearance: none;\n  font: inherit;\n  cursor: pointer;\n  border: 1px solid transparent;\n  border-radius: 8px;\n  padding: 5px 14px;\n  font-size: 13px;\n  line-height: 1.5;\n}\n\n.nyu-discard {\n  border-color: var(--dsw-alias-border-l2);\n  color: var(--dsw-alias-label-secondary);\n  background: transparent;\n}\n\n.nyu-discard:hover:not(:disabled) {\n  color: var(--dsw-alias-label-primary);\n  border-color: var(--dsw-alias-label-dimmed);\n}\n\n.nyu-save {\n  background: var(--dsw-alias-label-primary);\n  color: var(--dsw-alias-bg-layer-3);\n}\n\n.nyu-discard:disabled,\n.nyu-save:disabled {\n  opacity: 0.4;\n  cursor: default;\n}\n\n.nyu-discard:focus-visible,\n.nyu-save:focus-visible {\n  outline: 2px solid var(--dsw-alias-brand-primary);\n  outline-offset: 1px;\n}\n\n.nyu-field {\n  flex-direction: column;\n  gap: 6px;\n  padding: 12px 0;\n  display: flex;\n}\n\n.nyu-field + .nyu-field {\n  border-top: 1px solid var(--dsw-alias-border-l2);\n}\n\n.nyu-head {\n  align-items: center;\n  gap: 8px;\n  display: flex;\n}\n\n.nyu-label {\n  min-width: 0;\n  color: var(--dsw-alias-label-primary);\n  flex: 1;\n  font-size: 13px;\n  font-weight: 500;\n  line-height: 1.5;\n}\n\n.nyu-badges {\n  align-items: center;\n  gap: 8px;\n  display: inline-flex;\n}\n\n.nyu-badge {\n  white-space: nowrap;\n  background: var(--dsw-alias-bg-module-platform);\n  color: var(--dsw-alias-label-secondary);\n  border-radius: 999px;\n  padding: 1px 8px;\n  font-size: 11px;\n  font-weight: 500;\n  line-height: 17px;\n}\n\n.nyu-reset {\n  font: inherit;\n  color: var(--dsw-alias-label-secondary);\n  cursor: pointer;\n  background: transparent;\n  border: none;\n  padding: 0;\n  font-size: 12px;\n  line-height: 1.5;\n}\n\n.nyu-reset:hover:not(:disabled) {\n  color: var(--dsw-alias-label-primary);\n}\n\n.nyu-reset:disabled {\n  cursor: default;\n}\n\n.nyu-reset:focus-visible {\n  outline: 2px solid var(--dsw-alias-brand-primary);\n  outline-offset: 2px;\n}\n\n.nyu-reset:focus-visible {\n  outline: 2px solid var(--dsw-alias-brand-primary);\n  outline-offset: 2px;\n}\n\n.nyu-input,\n.nyu-select {\n  border: 1px solid var(--dsw-alias-border-l2);\n  background: var(--dsw-alias-bg-layer-3);\n  height: 34px;\n  font: inherit;\n  color: var(--dsw-alias-label-primary);\n  border-radius: 8px;\n  padding: 0 12px;\n  font-size: 13px;\n  line-height: 1.5;\n}\n\n.nyu-input:focus-visible,\n.nyu-select:focus-visible {\n  border-color: var(--dsw-alias-brand-primary);\n  outline: none;\n}\n\n.nyu-input:disabled,\n.nyu-select:disabled {\n  color: var(--dsw-alias-label-tertiary);\n  cursor: default;\n}\n\n.nyu-inputInvalid {\n  border: 1px solid var(--dsw-alias-label-error);\n  background: var(--dsw-alias-bg-layer-3);\n  height: 34px;\n  font: inherit;\n  color: var(--dsw-alias-label-primary);\n  border-radius: 8px;\n  padding: 0 12px;\n  font-size: 13px;\n  line-height: 1.5;\n}\n\n.nyu-inputInvalid:focus-visible {\n  outline: 2px solid var(--dsw-alias-label-error);\n  outline-offset: 1px;\n  border-color: var(--dsw-alias-label-error);\n}\n\n.nyu-invalid {\n  color: var(--dsw-alias-label-error);\n  margin: 0;\n  font-size: 12px;\n  line-height: 1.5;\n}\n\n.nyu-hint {\n  color: var(--dsw-alias-label-tertiary);\n  margin: 0;\n  font-size: 12px;\n  line-height: 1.5;\n}\n\n@media (prefers-reduced-motion: reduce) {\n  .nyu-card,\n  .nyu-header,\n  .nyu-chevron,\n  .nyu-chevronOpen,\n  .nyu-discard,\n  .nyu-save {\n    transition: none;\n  }\n}\n\n/* --- connection test row (nyro-usage extension of the family slice) --------- */\n\n.nyu-testRow {\n  align-items: center;\n  flex-wrap: wrap;\n  gap: 10px;\n  padding: 12px 0 4px;\n  display: flex;\n}\n\n.nyu-testButton {\n  appearance: none;\n  font: inherit;\n  cursor: pointer;\n  color: var(--dsw-alias-label-secondary);\n  border: 1px solid var(--dsw-alias-border-l2);\n  background: transparent;\n  border-radius: 8px;\n  padding: 5px 14px;\n  font-size: 13px;\n  line-height: 1.5;\n}\n\n.nyu-testButton:hover:not(:disabled) {\n  color: var(--dsw-alias-label-primary);\n  border-color: var(--dsw-alias-label-dimmed);\n}\n\n.nyu-testButton:disabled {\n  opacity: 0.4;\n  cursor: default;\n}\n\n.nyu-testButton:focus-visible {\n  outline: 2px solid var(--dsw-alias-brand-primary);\n  outline-offset: 1px;\n}\n\n.nyu-testOk {\n  color: var(--dsw-alias-label-success, var(--dsw-alias-state-success-primary));\n  font-size: 12px;\n  line-height: 1.5;\n}\n\n.nyu-testFail {\n  color: var(--dsw-alias-label-error, var(--dsw-alias-state-error-primary));\n  overflow-wrap: anywhere;\n  font-size: 12px;\n  line-height: 1.5;\n}\n";
if (typeof document !== "undefined") {
	const tag = "data-dsh-nyro-usage-css";
	if (document.querySelector("style[" + tag + "]") === null) {
		const el = document.createElement("style");
		el.setAttribute(tag, "");
		el.textContent = cssText;
		document.head.appendChild(el);
	}
}
var settings_card_module_css_default = {
	"card": "nyu-card",
	"cardOpen": "nyu-cardOpen",
	"header": "nyu-header",
	"headerStatic": "nyu-headerStatic",
	"headText": "nyu-headText",
	"name": "nyu-name",
	"description": "nyu-description",
	"pending": "nyu-pending",
	"chevron": "nyu-chevron",
	"chevronOpen": "nyu-chevronOpen",
	"body": "nyu-body",
	"readOnly": "nyu-readOnly",
	"notExposed": "nyu-notExposed",
	"footer": "nyu-footer",
	"failed": "nyu-failed",
	"discard": "nyu-discard",
	"save": "nyu-save",
	"field": "nyu-field",
	"head": "nyu-head",
	"label": "nyu-label",
	"badges": "nyu-badges",
	"badge": "nyu-badge",
	"reset": "nyu-reset",
	"input": "nyu-input",
	"select": "nyu-select",
	"inputInvalid": "nyu-inputInvalid",
	"invalid": "nyu-invalid",
	"hint": "nyu-hint",
	"testRow": "nyu-testRow",
	"testButton": "nyu-testButton",
	"testOk": "nyu-testOk",
	"testFail": "nyu-testFail"
};
//#endregion
//#region src/client/PluginSettingsCard.tsx
/**
* Family-shared chrome for plugin settings cards: a disclosure header naming
* the plugin and what its settings govern, the controls inside, and the save
* that writes them. Renders nothing while the namespace is unavailable — a
* deployment that does not compose the owning plugin should show no trace of
* it. Inlined into each consumer's client bundle; mirrors the official
* ui-plugin-config PluginCard in a self-contained slice.
*/
/**
* Render one plugin settings card.
* @param props - the plugin's copy keys, its form state, and its controls.
* @returns the card, or nothing while the namespace is still loading.
*/
function PluginSettingsCard(props) {
	const [open, setOpen] = (0, react.useState)(props.defaultOpen ?? true);
	const { state, alwaysOpen } = props;
	if (!state.available) return null;
	const title = props.t(props.titleKey);
	const description = props.t(props.descriptionKey);
	const blocked = !state.dirty || state.invalid || state.saving;
	const expanded = alwaysOpen === true || open;
	const cardClass = expanded ? `${settings_card_module_css_default.cardOpen} ${settings_card_module_css_default.card}` : settings_card_module_css_default.card;
	const header = alwaysOpen === true ? /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
		className: settings_card_module_css_default.headerStatic,
		children: [/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
			className: settings_card_module_css_default.headText,
			children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
				className: settings_card_module_css_default.name,
				title,
				children: title
			}), /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
				className: settings_card_module_css_default.description,
				title: description,
				children: description
			})]
		}), state.dirty ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
			className: settings_card_module_css_default.pending,
			title: props.t("settings.unsaved"),
			children: props.t("settings.unsaved")
		}) : null]
	}) : /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("button", {
		type: "button",
		className: settings_card_module_css_default.header,
		"aria-expanded": open,
		"aria-label": `${props.t(open ? "settings.collapse" : "settings.expand")}: ${title}`,
		onClick: () => {
			setOpen(!open);
		},
		children: [
			/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
				className: settings_card_module_css_default.headText,
				children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: settings_card_module_css_default.name,
					title,
					children: title
				}), /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: settings_card_module_css_default.description,
					title: description,
					children: description
				})]
			}),
			state.dirty ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
				className: settings_card_module_css_default.pending,
				title: props.t("settings.unsaved"),
				children: props.t("settings.unsaved")
			}) : null,
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)("svg", {
				width: "14",
				height: "14",
				viewBox: "0 0 14 14",
				fill: "none",
				xmlns: "http://www.w3.org/2000/svg",
				className: open ? `${settings_card_module_css_default.chevron} ${settings_card_module_css_default.chevronOpen}` : settings_card_module_css_default.chevron,
				children: /* @__PURE__ */ (0, react_jsx_runtime.jsx)("path", {
					d: "M11.8486 5.5L11.4238 5.92383L8.69727 8.65137C8.44157 8.90706 8.21562 9.13382 8.01172 9.29785C7.79912 9.46883 7.55595 9.61756 7.25 9.66602C7.08435 9.69222 6.91565 9.69222 6.75 9.66602C6.44405 9.61756 6.20088 9.46883 5.98828 9.29785C5.78438 9.13382 5.55843 8.90706 5.30273 8.65137L2.57617 5.92383L2.15137 5.5L3 4.65137L3.42383 5.07617L6.15137 7.80273C6.42595 8.07732 6.59876 8.24849 6.74023 8.3623C6.87291 8.46904 6.92272 8.47813 6.9375 8.48047C6.97895 8.48703 7.02105 8.48703 7.0625 8.48047C7.07728 8.47813 7.12709 8.46904 7.25977 8.3623C7.40124 8.24849 7.57405 8.07732 7.84863 7.80273L10.5762 5.07617L11 4.65137L11.8486 5.5Z",
					fill: "currentColor"
				})
			})
		]
	});
	if (!state.exposed) return /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("li", {
		className: cardClass,
		children: [header, expanded ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("div", {
			className: settings_card_module_css_default.body,
			children: /* @__PURE__ */ (0, react_jsx_runtime.jsx)("p", {
				className: settings_card_module_css_default.notExposed,
				role: "status",
				children: props.t("settings.notExposed")
			})
		}) : null]
	});
	return /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("li", {
		className: cardClass,
		children: [header, expanded ? /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
			className: settings_card_module_css_default.body,
			children: [
				!state.writable ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("p", {
					className: settings_card_module_css_default.readOnly,
					role: "status",
					children: props.t("settings.readOnly")
				}) : null,
				props.children,
				/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
					className: settings_card_module_css_default.footer,
					children: [
						state.failed ? /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("p", {
							className: settings_card_module_css_default.failed,
							role: "status",
							children: [props.t("settings.saveFailed"), state.failedReason ? " - " + state.failedReason : ""]
						}) : null,
						/* @__PURE__ */ (0, react_jsx_runtime.jsx)("button", {
							type: "button",
							className: settings_card_module_css_default.discard,
							disabled: !state.dirty || state.saving,
							onClick: props.onDiscard,
							children: props.t("settings.discard")
						}),
						/* @__PURE__ */ (0, react_jsx_runtime.jsx)("button", {
							type: "button",
							className: settings_card_module_css_default.save,
							disabled: blocked,
							onClick: props.onSave,
							children: props.t(!state.saving ? "settings.save" : "settings.saving")
						})
					]
				})
			]
		}) : null]
	});
}
/** A staged value field. `numeric` only hints the keypad: which drafts a field accepts is decided by its spec. */
function ValueField(props) {
	return /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
		className: settings_card_module_css_default.field,
		children: [
			/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
				className: settings_card_module_css_default.head,
				children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("label", {
					className: settings_card_module_css_default.label,
					htmlFor: props.id,
					children: props.label
				}), props.overridden ? /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
					className: settings_card_module_css_default.badges,
					children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
						className: settings_card_module_css_default.badge,
						children: props.overriddenLabel
					}), /* @__PURE__ */ (0, react_jsx_runtime.jsx)("button", {
						type: "button",
						className: settings_card_module_css_default.reset,
						disabled: props.disabled,
						onClick: props.onReset,
						children: props.resetLabel
					})]
				}) : null]
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)("input", {
				id: props.id,
				className: props.invalid ? settings_card_module_css_default.inputInvalid : settings_card_module_css_default.input,
				type: "text",
				...props.numeric === true ? { inputMode: "numeric" } : {},
				...props.invalid ? { "aria-invalid": true } : {},
				value: props.text,
				placeholder: props.placeholder ?? "",
				disabled: props.disabled,
				onChange: (event) => {
					props.onEdit(event.target.value);
				}
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)("p", {
				className: props.invalid ? settings_card_module_css_default.invalid : settings_card_module_css_default.hint,
				children: props.invalid ? props.invalidLabel : props.hint
			})
		]
	});
}
/** A staged boolean field: 继承 / 开 / 关. */
function BooleanField(props) {
	return /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
		className: settings_card_module_css_default.field,
		children: [
			/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
				className: settings_card_module_css_default.head,
				children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("label", {
					className: settings_card_module_css_default.label,
					htmlFor: props.id,
					children: props.label
				}), props.overridden ? /* @__PURE__ */ (0, react_jsx_runtime.jsxs)("span", {
					className: settings_card_module_css_default.badges,
					children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
						className: settings_card_module_css_default.badge,
						children: props.overriddenLabel
					}), /* @__PURE__ */ (0, react_jsx_runtime.jsx)("button", {
						type: "button",
						className: settings_card_module_css_default.reset,
						disabled: props.disabled,
						onClick: props.onReset,
						children: props.resetLabel
					})]
				}) : null]
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("select", {
				id: props.id,
				className: settings_card_module_css_default.select,
				value: props.text,
				disabled: props.disabled,
				onChange: (event) => {
					props.onEdit(event.target.value);
				},
				children: [
					/* @__PURE__ */ (0, react_jsx_runtime.jsx)("option", {
						value: "",
						children: props.inheritLabel
					}),
					/* @__PURE__ */ (0, react_jsx_runtime.jsx)("option", {
						value: "true",
						children: props.onLabel
					}),
					/* @__PURE__ */ (0, react_jsx_runtime.jsx)("option", {
						value: "false",
						children: props.offLabel
					})
				]
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)("p", {
				className: settings_card_module_css_default.hint,
				children: props.hint
			})
		]
	});
}
//#endregion
//#region src/client/settings-form.ts
/** A whole- or decimal-number field. An empty draft clears the field; any other draft that is not a finite number within the constraints blocks the save. */
function numberField(field, constraints = {}) {
	const { integer = false, min } = constraints;
	return {
		field,
		format: (value) => typeof value === "number" ? String(value) : "",
		parse: (text) => {
			const trimmed = text.trim();
			if (trimmed === "") return { kind: "clear" };
			const parsed = Number(trimmed);
			if (!Number.isFinite(parsed)) return void 0;
			if (integer && !Number.isInteger(parsed)) return void 0;
			if (min !== void 0 && parsed < min) return void 0;
			return {
				kind: "set",
				value: parsed
			};
		}
	};
}
/** A free-text field. An empty draft clears the field. */
function textField(field) {
	return {
		field,
		format: (value) => typeof value === "string" ? value : "",
		parse: (text) => {
			const trimmed = text.trim();
			return trimmed === "" ? { kind: "clear" } : {
				kind: "set",
				value: trimmed
			};
		}
	};
}
/**
* A free-text field the Host treats as a secret and redacts from the read-back
* (role('secret') in the section schema). The card still edits it like text,
* but a save never compares the redacted value back and relies on the scope
* reporting the write landed.
*/
function secretField(field) {
	return {
		...textField(field),
		secret: true
	};
}
/** A boolean field, edited through true/false draft text. */
function booleanField(field) {
	return {
		field,
		format: (value) => typeof value === "boolean" ? String(value) : "",
		parse: (text) => {
			const trimmed = text.trim();
			if (trimmed === "") return { kind: "clear" };
			if (trimmed === "true") return {
				kind: "set",
				value: true
			};
			if (trimmed === "false") return {
				kind: "set",
				value: false
			};
		}
	};
}
/**
* Stages one card's edits over one settings namespace and writes them on save.
*
* The Host is the only authority on whether a value was accepted — its
* validators own the constraints no schema can express — so the outcome is
* read back from the section rather than predicted here. A save that did not
* land keeps its drafts, so the user can correct them instead of retyping.
*/
var CardForm = class {
	scope;
	specs;
	staged = /* @__PURE__ */ new Map();
	listeners = /* @__PURE__ */ new Set();
	saving = false;
	failed = false;
	failedReason;
	/** @param scope - the bound settings scope for this card's namespace. */
	constructor(scope, specs) {
		this.scope = scope;
		this.specs = new Map(specs.map((spec) => [spec.field, spec]));
		scope.subscribe(() => {
			this.publish();
		});
	}
	/** Publish a projection of this form, rebuilt whenever the scope or a draft changes. */
	bind(project) {
		const store = (0, _deepseek_ai_dsh_client_runtime_client.createSnapshotStore)(project());
		this.listeners.add(() => {
			store.set(project());
		});
		return store;
	}
	/** Read the card-level state: what the Host serves, and what a save would do. */
	shell() {
		const snapshot = this.scope.getSnapshot();
		const plan = this.plan();
		return {
			available: snapshot.status !== "loading",
			exposed: snapshot.status === "ready",
			writable: snapshot.writable,
			dirty: plan.length > 0,
			invalid: plan.some((item) => item.run === void 0),
			saving: this.saving,
			failed: this.failed,
			...this.failedReason === void 0 ? {} : { failedReason: this.failedReason }
		};
	}
	/** Read one field's state from the effective section and its staged draft. */
	field(field) {
		const spec = this.specOf(field);
		const staged = this.staged.get(field);
		if (staged === void 0) return {
			text: spec.format(this.sectionValue(field)),
			overridden: this.stored(field),
			invalid: false
		};
		const write = staged.clear ? { kind: "clear" } : spec.parse(staged.text);
		return {
			text: staged.text,
			overridden: write?.kind === "set",
			invalid: write === void 0
		};
	}
	/** The actions the card's slot registration injects. */
	actions() {
		return {
			edit: (field, text) => {
				this.stage(field, {
					text,
					clear: false
				});
			},
			resetField: (field) => {
				this.stage(field, {
					text: this.specOf(field).format(this.baseValue(field)),
					clear: true
				});
			},
			save: () => {
				this.save();
			},
			discard: () => {
				if (this.staged.size === 0 && !this.failed) return;
				this.staged.clear();
				this.failed = false;
				this.failedReason = void 0;
				this.publish();
			}
		};
	}
	/**
	* Write every staged edit, then re-seed from what the Host accepted.
	*
	* When the scope carries the optional batch surface (the dsh-web-ui
	* bridge scope), every planned write rides one mutation so cross-field
	* validate hooks (baseURL+model) judge the batch as a unit instead of
	* deadlocking on per-field writes. Otherwise the per-field loop runs.
	* A field lands only when the Host reports it held the staged value; a
	* landed field's draft is dropped, a failed one stays staged for the user.
	* @returns settlement after every write and the read-back.
	*/
	async save() {
		const plan = this.plan();
		const valid = plan.filter((item) => item.run !== void 0);
		if (plan.length === 0 || this.saving || valid.length !== plan.length) return;
		const plannedWrites = valid.map((item) => item.op);
		const fields = new Set(plan.map((item) => item.field));
		this.saving = true;
		this.failed = false;
		this.failedReason = void 0;
		this.publish();
		const landed = /* @__PURE__ */ new Set();
		const batch = this.batchedScope();
		if (batch !== void 0) {
			const result = await batch.mutate(plannedWrites);
			if (result.ok) {
				for (const field of result.fields) if (field.landed) landed.add(field.field);
			} else this.failedReason = result.message;
		} else for (const item of valid) if (await item.run()) landed.add(item.field);
		for (const field of fields) if (landed.has(field)) this.staged.delete(field);
		this.saving = false;
		this.failed = landed.size !== fields.size;
		this.publish();
	}
	/** The scope's batch surface when it supports one; undefined conservatively otherwise. */
	batchedScope() {
		const candidate = this.scope;
		return typeof candidate?.mutate === "function" ? candidate : void 0;
	}
	/**
	* Every staged edit a save would write. An entry whose draft is not a value
	* its field accepts carries no write: the form is still dirty, and the save
	* refuses rather than dropping the edit. A staged edit that matches the
	* effective section is not a write at all.
	* @returns the planned writes, in the order the fields were staged.
	*/
	plan() {
		const plan = [];
		for (const [field, staged] of this.staged) {
			const spec = this.specOf(field);
			if (staged.clear) {
				if (this.stored(field)) plan.push({
					field,
					op: {
						field,
						op: "unset"
					},
					run: () => this.clear(field)
				});
				continue;
			}
			if (staged.text === spec.format(this.sectionValue(field))) continue;
			const write = spec.parse(staged.text);
			if (write === void 0) plan.push({
				field,
				op: {
					field,
					op: "unset"
				},
				run: void 0
			});
			else if (write.kind === "clear") plan.push({
				field,
				op: {
					field,
					op: "unset"
				},
				run: () => this.clear(field)
			});
			else plan.push({
				field,
				op: {
					field,
					op: "set",
					value: write.value
				},
				run: () => this.store(field, write.value)
			});
		}
		return plan;
	}
	async clear(field) {
		await this.scope.unset(field);
		return !this.stored(field);
	}
	async store(field, value) {
		await this.scope.set(field, value);
		if (this.specOf(field).secret) return true;
		return this.userLayer()?.[field] === value;
	}
	stage(field, edit) {
		this.staged.set(field, edit);
		this.failed = false;
		this.failedReason = void 0;
		this.publish();
	}
	specOf(field) {
		const spec = this.specs.get(field);
		if (spec === void 0) throw new Error(`settings card has no field ${field}`);
		return spec;
	}
	snapshotOf() {
		return this.scope.getSnapshot();
	}
	sectionValue(field) {
		return this.snapshotOf().value?.[field];
	}
	baseValue(field) {
		return this.snapshotOf().base?.[field];
	}
	userLayer() {
		return this.snapshotOf().user;
	}
	stored(field) {
		const user = this.userLayer();
		return user !== void 0 && Object.hasOwn(user, field);
	}
	publish() {
		for (const listener of this.listeners) listener();
	}
};
//#endregion
//#region src/client/NyroSettingsCard.tsx
/**
* The nyro-usage settings card: the connection (baseUrl + adminToken) and
* refresh parameters. Registers into the `web-ui.plugin.item` slot the
* plugin-configuration section renders, bound to the `nyro-usage` settings
* namespace. Includes a "test connection" action that exercises the saved
* configuration through the host proxy.
*/
/** Bridges the `nyro-usage` scope onto the card's staged form. */
var NyroUsageSettingsCardController = class {
	form;
	store;
	/** @param scope - the bound settings scope for the `nyro-usage` namespace. */
	constructor(scope) {
		this.form = new CardForm(scope, [
			booleanField("enabled"),
			textField("baseUrl"),
			secretField("adminToken"),
			numberField("refreshSeconds", {
				integer: true,
				min: 15
			}),
			numberField("cacheSeconds", {
				integer: true,
				min: 0
			})
		]);
		this.store = this.form.bind(() => this.projection());
	}
	projection() {
		return {
			...this.form.shell(),
			enabled: this.form.field("enabled"),
			baseUrl: this.form.field("baseUrl"),
			adminToken: this.form.field("adminToken"),
			refreshSeconds: this.form.field("refreshSeconds"),
			cacheSeconds: this.form.field("cacheSeconds")
		};
	}
	/**
	* Build the face the card's slot registration injects.
	* @returns the card's snapshot and its form actions.
	*/
	inject() {
		return {
			hooks: { nyroUsageSettingsCard: this.store },
			...this.form.actions()
		};
	}
};
/** Render the nyro-usage card. */
function NyroUsageSettingsCard(props) {
	const { t } = props;
	const state = props.useNyroUsageSettingsCard((snapshot) => snapshot);
	const disabled = !state.writable;
	const [testing, setTesting] = (0, react.useState)(false);
	const [testResult, setTestResult] = (0, react.useState)(null);
	const runTest = async () => {
		if (testing) return;
		setTesting(true);
		setTestResult(null);
		try {
			setTestResult(await new NyroPanelApi().test());
		} catch (error) {
			setTestResult({
				ok: false,
				kind: "network",
				message: error instanceof Error ? error.message : String(error)
			});
		} finally {
			setTesting(false);
		}
	};
	const fieldProps = {
		overriddenLabel: t("settings.overridden"),
		resetLabel: t("settings.reset"),
		invalidLabel: t("settings.invalidNumber"),
		disabled
	};
	return /* @__PURE__ */ (0, react_jsx_runtime.jsxs)(PluginSettingsCard, {
		t,
		titleKey: "settings.title",
		descriptionKey: "settings.description",
		state,
		onSave: props.save,
		onDiscard: props.discard,
		children: [
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)(BooleanField, {
				id: "settings-nyro-usage-enabled",
				label: t("settings.enabled"),
				hint: t("settings.enabledHint"),
				inheritLabel: t("settings.inherit"),
				onLabel: t("settings.on"),
				offLabel: t("settings.off"),
				...fieldProps,
				...state.enabled,
				onEdit: (text) => {
					props.edit("enabled", text);
				},
				onReset: () => {
					props.resetField("enabled");
				}
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)(ValueField, {
				id: "settings-nyro-usage-base-url",
				label: t("settings.baseUrl"),
				hint: t("settings.baseUrlHint"),
				placeholder: "http://192.168.31.2:19531",
				...fieldProps,
				...state.baseUrl,
				onEdit: (text) => {
					props.edit("baseUrl", text);
				},
				onReset: () => {
					props.resetField("baseUrl");
				}
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)(ValueField, {
				id: "settings-nyro-usage-admin-token",
				label: t("settings.adminToken"),
				hint: t("settings.adminTokenHint"),
				placeholder: "nyro NYRO_ADMIN_TOKEN",
				...fieldProps,
				...state.adminToken,
				onEdit: (text) => {
					props.edit("adminToken", text);
				},
				onReset: () => {
					props.resetField("adminToken");
				}
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)(ValueField, {
				id: "settings-nyro-usage-refresh",
				label: t("settings.refreshSeconds"),
				hint: t("settings.refreshSecondsHint"),
				numeric: true,
				...fieldProps,
				...state.refreshSeconds,
				onEdit: (text) => {
					props.edit("refreshSeconds", text);
				},
				onReset: () => {
					props.resetField("refreshSeconds");
				}
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsx)(ValueField, {
				id: "settings-nyro-usage-cache",
				label: t("settings.cacheSeconds"),
				hint: t("settings.cacheSecondsHint"),
				numeric: true,
				...fieldProps,
				...state.cacheSeconds,
				onEdit: (text) => {
					props.edit("cacheSeconds", text);
				},
				onReset: () => {
					props.resetField("cacheSeconds");
				}
			}),
			/* @__PURE__ */ (0, react_jsx_runtime.jsxs)("div", {
				className: settings_card_module_css_default.testRow,
				children: [/* @__PURE__ */ (0, react_jsx_runtime.jsx)("button", {
					type: "button",
					className: settings_card_module_css_default.testButton,
					disabled: testing || state.dirty || state.invalid,
					onClick: () => {
						runTest();
					},
					children: testing ? t("settings.testing") : t("settings.test")
				}), testResult !== null ? testResult.ok ? /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: settings_card_module_css_default.testOk,
					children: tt("settings.testOk", { count: testResult.providerCount ?? 0 })
				}) : /* @__PURE__ */ (0, react_jsx_runtime.jsx)("span", {
					className: settings_card_module_css_default.testFail,
					children: tt("settings.testFail", { message: testResult.message })
				}) : null]
			})
		]
	});
}
//#endregion
//#region src/client/controller.ts
var PanelController = class {
	open = false;
	/** Cached snapshot: useSyncExternalStore requires Object.is-stable reads. */
	snapshot = { panelOpen: false };
	listeners = /* @__PURE__ */ new Set();
	set(next) {
		if (this.open === next) return;
		this.open = next;
		this.snapshot = { panelOpen: next };
		for (const listener of this.listeners) listener();
	}
	toggle = () => {
		this.set(!this.open);
	};
	close = () => {
		this.set(false);
	};
	getSnapshot = () => this.snapshot;
	subscribe = (listener) => {
		this.listeners.add(listener);
		return () => {
			this.listeners.delete(listener);
		};
	};
};
//#endregion
//#region src/client/sidebar-entry.ts
/** Entry-row styles (single source: the panel stylesheet injects them). */
/** Sidebar family rows injected by sibling plugins (ordering block). */
const FAMILY_SELECTOR = "[data-dsh-taskboard-entry], [data-dsh-ssh-entry], [data-dsh-nyro-usage-entry]";
/** Find the sidebar shell root element, or undefined while not yet mounted. */
function sidebarRoot() {
	const column = document.querySelector("[data-pane=\"sidebar\"], [class*=\"sidebarCol\"]");
	if (column === null) return void 0;
	return column.querySelector("[class*=\"logoRow\"]")?.parentElement ?? column.firstElementChild;
}
/** The New Session button: nested in the logo row on current shells, a direct child on legacy shells. */
function newSessionButton(root) {
	const nested = root.querySelector("button[class*=\"newSession\"]");
	if (nested !== null) return nested;
	for (const child of root.children) if (child.tagName === "BUTTON") return child;
}
/** Build the entry row (a detached button; insert once the shell is up). */
function createEntry(controller, label, tooltip) {
	const entry = document.createElement("button");
	entry.type = "button";
	entry.dataset.dshNyroUsageEntry = "";
	entry.className = panel_module_css_default.entry;
	entry.setAttribute("aria-label", label);
	entry.setAttribute("title", tooltip);
	entry.innerHTML = "<span class=\"" + panel_module_css_default.entryIcon + "\"><svg viewBox=\"0 0 16 16\" width=\"14\" height=\"14\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.3\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\"><path d=\"M3 11.5a5 5 0 1 1 10 0\"/><path d=\"M8 8.4 10.6 6\"/><path d=\"M8 11.5h.01\"/><path d=\"M1.5 11.5h1M13.5 11.5h1M8 3.5v-1\"/></svg></span><span class=\"" + panel_module_css_default.entryLabel + "\">" + label + "</span>";
	entry.addEventListener("click", () => {
		controller.toggle();
	});
	return entry;
}
/** Re-insert the entry after the family block (before the browser region). */
function placeEntry(root, entry) {
	const button = newSessionButton(root);
	if (button === void 0) return false;
	if (entry.parentElement !== root) {
		const row = button.closest("[class*=\"logoRow\"]");
		const base = row !== null && row.parentElement === root ? row : button;
		const family = Array.from(root.children).filter((el) => el instanceof HTMLElement && el.matches(FAMILY_SELECTOR));
		const anchor = family.length > 0 ? family[family.length - 1].nextElementSibling : base.nextElementSibling;
		root.insertBefore(entry, anchor);
	}
	return true;
}
/**
* Mount the sidebar entry, waiting for the shell to render and self-healing
* on later React re-renders.
* @param controller - the panel controller the entry toggles.
* @param label - visible entry label.
* @param tooltip - entry tooltip.
* @returns disposer removing the entry and its observers.
*/
function mountSidebarEntry(controller, label, tooltip) {
	const entry = createEntry(controller, label, tooltip);
	let root;
	let placed = false;
	const tryPlace = () => {
		if (root !== void 0 && !root.isConnected) {
			rootObserver.disconnect();
			root = void 0;
			placed = false;
		}
		if (placed) {
			if (document.body.contains(entry)) return;
			rootObserver.disconnect();
			root = void 0;
			placed = false;
		}
		root ??= sidebarRoot();
		if (root === void 0) return;
		placed = placeEntry(root, entry);
		if (placed) rootObserver.observe(root, {
			childList: true,
			subtree: true
		});
	};
	const waitObserver = new MutationObserver(() => {
		tryPlace();
	});
	waitObserver.observe(document.body, {
		childList: true,
		subtree: true
	});
	const rootObserver = new MutationObserver(() => {
		if (root === void 0 || !root.isConnected) {
			placed = false;
			tryPlace();
			return;
		}
		if (!root.contains(entry)) placed = placeEntry(root, entry);
	});
	const syncActive = () => {
		if (controller.getSnapshot().panelOpen) entry.dataset.active = "true";
		else delete entry.dataset.active;
	};
	const unsubscribe = controller.subscribe(syncActive);
	syncActive();
	tryPlace();
	return () => {
		waitObserver.disconnect();
		rootObserver.disconnect();
		unsubscribe();
		entry.remove();
	};
}
//#endregion
//#region src/client/index.ts
/** Locale namespace this plugin owns. */
const NS = "nyro-usage";
/** Settings namespace the nyro-usage card edits (the Host plugin registers it). */
const NYRO_USAGE_NS = "nyro-usage";
/** Required services (fiber inject waiting — the runtime must be up first). */
const inject = [
	"slots",
	"locale",
	"connection",
	"settingsScope",
	"remote"
];
/**
* Mount the nyro usage surfaces.
* @param ctx - client root context (locale + settings services).
*/
function apply(ctx) {
	ctx.effect(() => ctx.locale.register(NS, {
		zh,
		en
	}), "nyro-usage: dictionaries");
	const controller = new PanelController();
	const api = new NyroPanelApi();
	const disposers = [];
	try {
		disposers.push(mountSidebarEntry(controller, tt("entry.label"), tt("entry.tooltip")));
		disposers.push(mountPanel(controller, api));
	} catch (error) {
		console.error("[dsh-nyro-usage] panel mount failed", error);
	}
	ctx.effect(() => () => {
		for (const dispose of disposers.splice(0)) dispose();
	}, "nyro-usage: surfaces");
	const settings = new NyroUsageSettingsCardController((ctx.get("webUiSettings") ?? ctx.settingsScope).bind({ namespace: NYRO_USAGE_NS }));
	ctx.slots.inject("web-ui.plugin.item", () => ctx.slots.register({
		name: "web-ui.plugin.item",
		id: "nyro-usage",
		order: 120,
		locale: NS,
		inject: () => settings.inject()
	}, NyroUsageSettingsCard));
}
//#endregion
exports.apply = apply;
exports.inject = inject;

		return module.exports;
	}
});