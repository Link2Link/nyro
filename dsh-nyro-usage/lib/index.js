import { installSettingsSection, settingsNamespace } from "@deepseek-ai/dsh-settings";
import z from "schemastery";
//#region src/nyro.ts
/**
* Normalize a configured base URL: trims whitespace, drops trailing slashes
* and a trailing `/api/v1` (operators paste either shape). Returns '' when
* nothing usable remains.
*/
function normalizeBaseUrl(raw) {
	const trimmed = (raw ?? "").trim();
	if (trimmed === "") return "";
	let url = trimmed.replace(/\/+$/, "");
	if (url.endsWith("/api/v1")) url = url.slice(0, -7).replace(/\/+$/, "");
	return url;
}
/** Errors carrying a machine-readable classification for route mapping. */
var NyroApiError = class extends Error {
	kind;
	constructor(message, kind) {
		super(message);
		this.kind = kind;
		this.name = "NyroApiError";
	}
};
/** Fetch timeout: nyro's bulk usage queries upstreams concurrently (4-way)
* with per-request timeouts of 15–20s, so the whole call can legitimately
* take tens of seconds. */
const USAGE_TIMEOUT_MS = 7e4;
const TEST_TIMEOUT_MS = 1e4;
/** Reject non-http(s) schemes early (file:, etc. must never be fetched). */
function assertHttpUrl(base) {
	let parsed;
	try {
		parsed = new URL(base);
	} catch {
		throw new NyroApiError(`invalid baseUrl: ${base}`, "bad-url");
	}
	if (parsed.protocol !== "http:" && parsed.protocol !== "https:") throw new NyroApiError(`baseUrl must be http(s): ${base}`, "bad-url");
}
/** Shared fetch + error classification for the nyro admin plane. */
async function nyroFetch(base, token, path, timeoutMs) {
	if (base === "") throw new NyroApiError("baseUrl is not configured", "unconfigured");
	if (token.trim() === "") throw new NyroApiError("adminToken is not configured", "unconfigured");
	assertHttpUrl(base);
	let response;
	try {
		response = await fetch(`${base}${path}`, {
			headers: { authorization: `Bearer ${token}` },
			signal: AbortSignal.timeout(timeoutMs)
		});
	} catch (error) {
		const reason = error instanceof Error ? error.message : String(error);
		throw new NyroApiError(error instanceof Error && error.name === "TimeoutError" ? `nyro unreachable (timeout): ${base}` : `nyro unreachable: ${reason}`, "network");
	}
	if (response.status === 401) throw new NyroApiError("invalid admin token (HTTP 401)", "unauthorized");
	let body;
	try {
		body = await response.json();
	} catch {
		body = void 0;
	}
	const bodyError = typeof body === "object" && body !== null ? body.error : void 0;
	if (response.status === 404 || typeof bodyError === "string" && bodyError.includes("provider not found: usage")) throw new NyroApiError("nyro has no GET /api/v1/providers/usage endpoint (version too old); upgrade nyro", "not-found");
	if (!response.ok) throw new NyroApiError(`nyro error: ${typeof bodyError === "string" && bodyError !== "" ? bodyError : `HTTP ${response.status}`}`, "server");
	return body;
}
/** Client for one configured nyro gateway. Cheap to construct per request. */
var NyroClient = class {
	baseUrl;
	adminToken;
	constructor(baseUrl, adminToken) {
		this.baseUrl = baseUrl;
		this.adminToken = adminToken;
	}
	/** Fetch every provider's usage (the bulk endpoint). */
	async usage() {
		const body = await nyroFetch(this.baseUrl, this.adminToken, "/api/v1/providers/usage", USAGE_TIMEOUT_MS);
		if (typeof body !== "object" || body === null || !Array.isArray(body.data)) throw new NyroApiError("unexpected nyro response: missing data array", "server");
		return body;
	}
	/** Cheap authenticated call used by the connection test. */
	async test() {
		const body = await nyroFetch(this.baseUrl, this.adminToken, "/api/v1/providers", TEST_TIMEOUT_MS);
		const count = typeof body === "object" && body !== null && Array.isArray(body.data) ? body.data.length : void 0;
		return {
			ok: true,
			kind: "reachable",
			message: "connected",
			...count === void 0 ? {} : { providerCount: count }
		};
	}
};
//#endregion
//#region src/routes.ts
/** Route paths this family owns. */
const NYRO_API = {
	usage: "/api/nyro-usage/usage",
	status: "/api/nyro-usage/status",
	test: "/api/nyro-usage/test"
};
/** Loopback literal check plus browser same-origin markers (mirrors the
* dsh-ssh pairing-routes fence: LAN-exposed dsh web deployments must not
* serve the usage proxy to remote callers). */
function isLoopbackRequest(request) {
	const address = request.socket.remoteAddress;
	if (address !== "127.0.0.1" && address !== "::1" && address !== "::ffff:127.0.0.1") return false;
	const host = request.headers.host;
	if (typeof host !== "string") return false;
	let hostUrl;
	try {
		hostUrl = new URL(`http://${host}`);
	} catch {
		return false;
	}
	if (hostUrl.hostname !== "127.0.0.1" && hostUrl.hostname !== "localhost" && hostUrl.hostname !== "[::1]") return false;
	if (request.headers["sec-fetch-site"] === "cross-site") return false;
	const origin = request.headers.origin;
	if (origin === void 0) return true;
	try {
		return new URL(origin).host === hostUrl.host;
	} catch {
		return false;
	}
}
/** One JSON response. */
function writeJson(res, status, body) {
	const payload = JSON.stringify(body);
	res.writeHead(status, {
		"content-type": "application/json; charset=utf-8",
		"referrer-policy": "no-referrer"
	});
	res.end(payload);
}
/** Map a NyroApiError to an HTTP status the panel can branch on. */
function errorStatus(kind) {
	switch (kind) {
		case "unconfigured": return 400;
		case "bad-url": return 400;
		case "unauthorized": return 401;
		case "not-found": return 501;
		case "network": return 502;
		default: return 502;
	}
}
/** Cached bulk-usage snapshot with in-flight dedupe. */
var UsageCache = class {
	entry;
	inflight;
	/**
	* Read the bulk usage, serving the cached copy while it is fresh enough.
	* Concurrent readers share one upstream call.
	* @param config - resolved plugin config (client + TTL).
	* @param force - bypass the cache (manual refresh).
	* @returns the snapshot plus its fetch time.
	*/
	read(config, force) {
		const ttl = Math.max(0, config.cacheSeconds) * 1e3;
		if (!force && this.entry !== void 0 && Date.now() - this.entry.at < ttl) return Promise.resolve(this.entry);
		this.inflight ??= new NyroClient(config.baseUrl, config.adminToken).usage().then((data) => {
			const snapshot = {
				at: Date.now(),
				data
			};
			this.entry = snapshot;
			return snapshot;
		}).finally(() => {
			this.inflight = void 0;
		});
		return this.inflight;
	}
	/** The cached fetch time, if any (for the status route). */
	cachedAt() {
		return this.entry?.at;
	}
	/** Drop the cache (config change: the token/baseUrl may have changed). */
	invalidate() {
		this.entry = void 0;
	}
};
/**
* Build the /api/nyro-usage route family.
* @param deps - config source.
* @returns the routes to register on the webserver.
*/
function makeRoutes(deps) {
	const cache = new UsageCache();
	let lastSignature = "";
	const signature = () => {
		const value = deps.config();
		return `${value.baseUrl}\u0000${value.adminToken}`;
	};
	const clientOf = () => {
		const value = deps.config();
		const next = signature();
		if (next !== lastSignature) {
			cache.invalidate();
			lastSignature = next;
		}
		return new NyroClient(value.baseUrl, value.adminToken);
	};
	const guard = (req, res) => {
		if (!isLoopbackRequest(req)) {
			writeJson(res, 403, { error: "forbidden: loopback-only" });
			return false;
		}
		return true;
	};
	return [
		{
			kind: "exact",
			path: NYRO_API.usage,
			handler: async (req, res) => {
				if (!guard(req, res)) return;
				if (req.method !== "GET") {
					writeJson(res, 405, { error: `method not allowed: ${req.method}` });
					return;
				}
				const value = deps.config();
				if (value.baseUrl === "" || value.adminToken.trim() === "") {
					writeJson(res, 400, { error: "nyro baseUrl / adminToken not configured" });
					return;
				}
				const force = new URL(req.url ?? "/", "http://localhost").searchParams.get("refresh") === "1";
				try {
					clientOf();
					const snapshot = await cache.read(value, force);
					writeJson(res, 200, {
						data: snapshot.data.data ?? [],
						cachedAt: new Date(snapshot.at).toISOString()
					});
				} catch (error) {
					if (error instanceof NyroApiError) {
						writeJson(res, errorStatus(error.kind), {
							error: error.message,
							kind: error.kind
						});
						return;
					}
					writeJson(res, 502, { error: error instanceof Error ? error.message : String(error) });
				}
			}
		},
		{
			kind: "exact",
			path: NYRO_API.status,
			handler: (req, res) => {
				if (!guard(req, res)) return;
				if (req.method !== "GET") {
					writeJson(res, 405, { error: `method not allowed: ${req.method}` });
					return;
				}
				const value = deps.config();
				const cachedAt = cache.cachedAt();
				writeJson(res, 200, {
					configured: value.baseUrl !== "" && value.adminToken.trim() !== "",
					baseUrl: value.baseUrl,
					hasToken: value.adminToken.trim() !== "",
					refreshSeconds: value.refreshSeconds,
					cacheSeconds: value.cacheSeconds,
					cachedAt: cachedAt === void 0 ? null : new Date(cachedAt).toISOString()
				});
			}
		},
		{
			kind: "exact",
			path: NYRO_API.test,
			handler: async (req, res) => {
				if (!guard(req, res)) return;
				if (req.method !== "POST") {
					writeJson(res, 405, { error: `method not allowed: ${req.method}` });
					return;
				}
				try {
					writeJson(res, 200, await clientOf().test());
				} catch (error) {
					if (error instanceof NyroApiError) {
						writeJson(res, 200, {
							ok: false,
							kind: error.kind,
							message: error.message
						});
						return;
					}
					writeJson(res, 200, {
						ok: false,
						kind: "network",
						message: error instanceof Error ? error.message : String(error)
					});
				}
			}
		}
	];
}
//#endregion
//#region src/index.ts
/** Stable cordis plugin name. */
const name = "nyro-usage";
/** Services required before the proxy routes can mount. */
const inject = ["webServer"];
/**
* Settings namespace of the nyro-usage capability — the section the web
* settings surface edits. Spelled here rather than imported: the browser
* half spells the same value and must not depend on a Host package.
*/
const NYRO_USAGE_SETTINGS_NAMESPACE = settingsNamespace("nyro-usage");
const Config = z.object({
	enabled: z.boolean().default(true),
	baseUrl: z.string().default(""),
	adminToken: z.string().role("secret").default(""),
	refreshSeconds: z.number().min(15).default(300),
	cacheSeconds: z.number().min(0).default(30)
});
/**
* Mount the nyro usage proxy routes.
* @param ctx - host plugin context carrying webServer.
* @param config - resolved plugin config (schema defaults applied by the loader).
*/
function apply(ctx, config = {}) {
	let current = () => config ?? {};
	const resolve = () => {
		const value = current();
		return {
			baseUrl: normalizeBaseUrl(value.baseUrl),
			adminToken: value.adminToken ?? "",
			refreshSeconds: value.refreshSeconds ?? 300,
			cacheSeconds: value.cacheSeconds ?? 30
		};
	};
	let disposeRoutes;
	const sync = () => {
		if (disposeRoutes !== void 0) {
			disposeRoutes();
			disposeRoutes = void 0;
		}
		if ((current().enabled ?? true) === false) return;
		const disposers = makeRoutes({ config: resolve }).map((route) => ctx.webServer.register(route));
		disposeRoutes = () => {
			for (const dispose of disposers) dispose();
		};
		ctx.effect(() => () => {
			disposeRoutes?.();
		}, "nyro-usage: routes");
	};
	installSettingsSection(ctx, NYRO_USAGE_SETTINGS_NAMESPACE, Config, config ?? {}, {
		setSource: (source) => {
			current = source;
		},
		onChange: sync
	});
	sync();
}
//#endregion
export { Config, NYRO_USAGE_SETTINGS_NAMESPACE, apply, inject, name };
