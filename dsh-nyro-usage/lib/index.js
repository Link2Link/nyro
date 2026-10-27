import z from "@deepseek-ai/schemastery";
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
	/** `unauthorized` | `not-found` | `network` | `bad-url` | `unconfigured` | `server`. */
	kind;
	constructor(message, kind) {
		super(message);
		this.name = "NyroApiError";
		this.kind = kind;
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
//#region src/volatile.ts
/**
* Read a config field that may arrive as a plain scalar (plain
* `schemastery` resolution — what this package depends on) or as a
* volatile reference (the `@deepseek-ai/schemastery` fork used inside the
* dsh host). Kept tolerant so either resolution feeds the routes.
*/
function isVolatileRef(value) {
	return typeof value === "object" && value !== null && typeof value.get === "function";
}
function derefVolatile(value, fallback) {
	if (isVolatileRef(value)) {
		const inner = value.get();
		return inner === void 0 || inner === null ? fallback : inner;
	}
	return value === void 0 || value === null ? fallback : value;
}
//#endregion
//#region src/instances.ts
/**
* Multi-gateway resolution: turns the plugin config into the ordered list of
* nyro gateways the routes query.
*
* Two config shapes coexist, so existing single-gateway configs keep working
* untouched:
* - the primary gateway: the top-level `baseUrl` / `adminToken` (+ optional
*   `name`) — always id `default`, listed first;
* - additional gateways: the `instances[]` list, each
*   `{ name, baseUrl, adminToken, enabled }`.
*
* Ids are URL/localStorage-safe slugs of the display name: the panel keys
* sections, per-gateway card order and `?instance=` requests by them.
*/
/** Id of the primary (top-level) gateway. */
const PRIMARY_INSTANCE_ID = "default";
/** Read a possibly-volatile string field, trimmed. */
function str(value) {
	const plain = derefVolatile(value, "");
	return typeof plain === "string" ? plain.trim() : "";
}
/** The `instances[]` rows as plain records (non-object rows become `{}`, keeping indices aligned). */
function rowsOf(source) {
	const list = derefVolatile(source.instances, []);
	if (!Array.isArray(list)) return [];
	return list.map((raw) => {
		const row = derefVolatile(raw, void 0);
		return typeof row === "object" && row !== null ? row : {};
	});
}
/** Display fallback: the base URL's host, or the URL itself. */
function hostLabel(baseUrl) {
	try {
		return new URL(baseUrl).host || baseUrl;
	} catch {
		return baseUrl;
	}
}
/**
* Slugify a display name into an id: lowercase, unicode letters/digits kept,
* every other run collapsed to `-`. '' when nothing usable remains.
* @param name - the display name.
* @returns the slug.
*/
function slugifyInstanceName(name) {
	return name.trim().toLowerCase().replace(/[^\p{L}\p{N}]+/gu, "-").replace(/^-+|-+$/g, "");
}
/**
* Resolve the configured gateways, primary first, then `instances[]` in
* order. Rows without a base URL and rows with `enabled: false` are skipped;
* a row without a token is kept so the panel can report it. Ids are made
* unique with `-2`, `-3`… suffixes; `default` is reserved for the primary.
* @param source - raw plugin config (scalars or volatile refs).
* @returns the gateways the routes should query.
*/
function resolveInstances(source) {
	const result = [];
	const taken = /* @__PURE__ */ new Set([PRIMARY_INSTANCE_ID]);
	const primaryUrl = normalizeBaseUrl(str(source.baseUrl));
	if (primaryUrl !== "") result.push({
		id: PRIMARY_INSTANCE_ID,
		name: str(source.name) || hostLabel(primaryUrl),
		baseUrl: primaryUrl,
		adminToken: str(source.adminToken)
	});
	rowsOf(source).forEach((row, index) => {
		if (derefVolatile(row.enabled, true) === false) return;
		const baseUrl = normalizeBaseUrl(str(row.baseUrl));
		if (baseUrl === "") return;
		const name = str(row.name);
		const stem = slugifyInstanceName(name) || `nyro-${index + 1}`;
		let id = stem;
		for (let n = 2; taken.has(id); n++) id = `${stem}-${n}`;
		taken.add(id);
		result.push({
			id,
			name: name || hostLabel(baseUrl),
			baseUrl,
			adminToken: str(row.adminToken)
		});
	});
	return result;
}
/**
* Report which configured gateways hold a token, by config position.
* @param source - raw plugin config (scalars or volatile refs).
* @returns token presence for the primary and each `instances[]` row.
*/
function tokenFlags(source) {
	return {
		primary: str(source.adminToken) !== "",
		rows: rowsOf(source).map((row) => str(row.adminToken) !== "")
	};
}
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
/** Cached bulk-usage snapshot of ONE gateway with in-flight dedupe. */
var UsageCache = class {
	instance;
	entry;
	inflight;
	constructor(instance) {
		this.instance = instance;
	}
	/**
	* Read the bulk usage, serving the cached copy while it is fresh enough.
	* Concurrent readers share one upstream call.
	* @param ttlSeconds - cache TTL (0 disables caching).
	* @param force - bypass the cache (manual refresh).
	* @returns the snapshot plus its fetch time.
	*/
	read(ttlSeconds, force) {
		const ttl = Math.max(0, ttlSeconds) * 1e3;
		if (!force && this.entry !== void 0 && Date.now() - this.entry.at < ttl) return Promise.resolve(this.entry);
		this.inflight ??= new NyroClient(this.instance.baseUrl, this.instance.adminToken).usage().then((data) => {
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
};
/** Cache identity: a gateway whose URL or token changed must not serve the
* previous gateway's data. */
function signatureOf(instance) {
	return `${instance.id}\u0000${instance.baseUrl}\u0000${instance.adminToken}`;
}
/** Error text + kind of anything a gateway call threw. */
function describeError(error) {
	if (error instanceof NyroApiError) return {
		message: error.message,
		kind: error.kind
	};
	return {
		message: error instanceof Error ? error.message : String(error),
		kind: "network"
	};
}
/**
* Build the /api/nyro-usage route family.
* @param deps - config source.
* @returns the routes to register on the webserver.
*/
function makeRoutes(deps) {
	/** One cache per gateway signature; stale signatures are pruned per request. */
	const caches = /* @__PURE__ */ new Map();
	const current = () => {
		const value = deps.config();
		const live = new Set(value.instances.map(signatureOf));
		for (const key of caches.keys()) if (!live.has(key)) caches.delete(key);
		return value;
	};
	const cacheOf = (instance) => {
		const key = signatureOf(instance);
		let cache = caches.get(key);
		if (cache === void 0) {
			cache = new UsageCache(instance);
			caches.set(key, cache);
		}
		return cache;
	};
	/** Narrow to `?instance=<id>` when given; undefined = unknown id. */
	const targetsOf = (url, value) => {
		const wanted = url.searchParams.get("instance");
		if (wanted === null || wanted === "") return value.instances;
		const match = value.instances.find((instance) => instance.id === wanted);
		return match === void 0 ? void 0 : [match];
	};
	const guard = (req, res, method) => {
		if (!isLoopbackRequest(req)) {
			writeJson(res, 403, { error: "forbidden: loopback-only" });
			return;
		}
		if (req.method !== method) {
			writeJson(res, 405, { error: `method not allowed: ${req.method}` });
			return;
		}
		return new URL(req.url ?? "/", "http://localhost");
	};
	const readInstance = async (instance, ttl, force) => {
		const head = {
			id: instance.id,
			name: instance.name,
			baseUrl: instance.baseUrl
		};
		if (instance.adminToken === "") return {
			...head,
			items: [],
			cachedAt: null,
			error: "adminToken is not configured",
			errorKind: "unconfigured"
		};
		try {
			const snapshot = await cacheOf(instance).read(ttl, force);
			return {
				...head,
				items: snapshot.data.data ?? [],
				cachedAt: new Date(snapshot.at).toISOString(),
				error: null,
				errorKind: null
			};
		} catch (error) {
			const { message, kind } = describeError(error);
			return {
				...head,
				items: [],
				cachedAt: null,
				error: message,
				errorKind: kind
			};
		}
	};
	const testInstance = async (instance) => {
		const head = {
			id: instance.id,
			name: instance.name,
			baseUrl: instance.baseUrl
		};
		try {
			return {
				...head,
				...await new NyroClient(instance.baseUrl, instance.adminToken).test()
			};
		} catch (error) {
			const { message, kind } = describeError(error);
			return {
				...head,
				ok: false,
				kind,
				message
			};
		}
	};
	return [
		{
			kind: "exact",
			path: NYRO_API.usage,
			handler: async (req, res) => {
				const url = guard(req, res, "GET");
				if (url === void 0) return;
				const value = current();
				if (!value.enabled) {
					writeJson(res, 503, {
						error: "nyro-usage plugin is disabled",
						kind: "disabled"
					});
					return;
				}
				if (value.instances.length === 0) {
					writeJson(res, 400, {
						error: "no nyro gateway configured (baseUrl / instances)",
						kind: "unconfigured"
					});
					return;
				}
				const targets = targetsOf(url, value);
				if (targets === void 0) {
					writeJson(res, 404, {
						error: "unknown nyro instance",
						kind: "not-found"
					});
					return;
				}
				const force = url.searchParams.get("refresh") === "1";
				writeJson(res, 200, { instances: await Promise.all(targets.map((instance) => readInstance(instance, value.cacheSeconds, force))) });
			}
		},
		{
			kind: "exact",
			path: NYRO_API.status,
			handler: (req, res) => {
				if (guard(req, res, "GET") === void 0) return;
				const value = current();
				const instances = value.instances.map((instance) => {
					const at = caches.get(signatureOf(instance))?.cachedAt();
					return {
						id: instance.id,
						name: instance.name,
						baseUrl: instance.baseUrl,
						hasToken: instance.adminToken !== "",
						cachedAt: at === void 0 ? null : new Date(at).toISOString()
					};
				});
				writeJson(res, 200, {
					configured: value.enabled && instances.length > 0,
					disabled: !value.enabled,
					refreshSeconds: value.refreshSeconds,
					cacheSeconds: value.cacheSeconds,
					instances,
					tokens: {
						primary: value.tokens.primary,
						rows: [...value.tokens.rows]
					}
				});
			}
		},
		{
			kind: "exact",
			path: NYRO_API.test,
			handler: async (req, res) => {
				const url = guard(req, res, "POST");
				if (url === void 0) return;
				const value = current();
				if (!value.enabled) {
					writeJson(res, 200, {
						ok: false,
						kind: "disabled",
						message: "nyro-usage plugin is disabled",
						instances: []
					});
					return;
				}
				const targets = targetsOf(url, value);
				if (targets === void 0 || targets.length === 0) {
					writeJson(res, 200, {
						ok: false,
						kind: targets === void 0 ? "not-found" : "unconfigured",
						message: targets === void 0 ? "unknown nyro instance" : "no nyro gateway configured (baseUrl / instances)",
						instances: []
					});
					return;
				}
				const instances = await Promise.all(targets.map(testInstance));
				const failed = instances.find((result) => !result.ok);
				writeJson(res, 200, failed === void 0 ? {
					ok: true,
					kind: "reachable",
					message: "connected",
					providerCount: instances.reduce((sum, result) => sum + (result.providerCount ?? 0), 0),
					instances
				} : {
					ok: false,
					kind: failed.kind,
					message: `${failed.name}: ${failed.message}`,
					instances
				});
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
* Settings namespace of the nyro-usage capability — the profile entry id
* (the dsh ≥ 0.1.7 settings key). Spelled here rather than derived: the
* browser half spells the same value and must not depend on a Host package.
*/
const NYRO_USAGE_SETTINGS_NAMESPACE = "nyro-usage";
/** Schema of one `instances[]` row. */
const InstanceSchema = z.object({
	name: z.string().default(""),
	baseUrl: z.string().default(""),
	adminToken: z.string().role("secret").default(""),
	enabled: z.boolean().default(true)
});
/**
* Module-level Config: dsh ≥ 0.1.7 derives the settings section from this
* export (namespace = entry id). Every field is volatile: the
* `@deepseek-ai/schemastery` fork resolves volatile fields to references the
* loader updates in place on a settings write, so edits apply without
* remounting the fiber; reads go through `derefVolatile`.
*/
const Config = z.object({
	enabled: z.boolean().default(true).extra("volatile", true),
	name: z.string().default("").extra("volatile", true),
	baseUrl: z.string().default("").extra("volatile", true),
	adminToken: z.string().role("secret").default("").extra("volatile", true),
	instances: z.array(InstanceSchema).default([]).extra("volatile", true),
	refreshSeconds: z.number().min(15).default(300).extra("volatile", true),
	cacheSeconds: z.number().min(0).default(30).extra("volatile", true)
});
/**
* Resolve the route config from the plugin config (scalars or volatile refs).
* @param value - the resolved plugin config.
* @returns what the routes read per request.
*/
function resolveRouteConfig(value) {
	return {
		enabled: derefVolatile(value.enabled, true) !== false,
		instances: resolveInstances(value),
		tokens: tokenFlags(value),
		refreshSeconds: derefVolatile(value.refreshSeconds, 300),
		cacheSeconds: derefVolatile(value.cacheSeconds, 30)
	};
}
/**
* Mount the nyro usage proxy routes.
* @param ctx - host plugin context carrying webServer.
* @param config - resolved plugin config (schema defaults applied by the loader).
*/
function apply(ctx, config = {}) {
	if (ctx.webServer === void 0) return;
	const disposers = makeRoutes({ config: () => resolveRouteConfig(config ?? {}) }).map((route) => ctx.webServer.register(route));
	ctx.effect(() => () => {
		for (const dispose of disposers) dispose();
	}, "nyro-usage: routes");
}
//#endregion
export { Config, NYRO_USAGE_SETTINGS_NAMESPACE, apply, inject, name, resolveRouteConfig };
