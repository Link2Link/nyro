# dsh-nyro-usage

[Nyro](https://github.com/Link2Link/nyro) provider usage panel for DSH Web.

Adds a **Nyro Usage** entry to the dsh web GUI sidebar and renders every
configured nyro provider's upstream usage in one place: coding-plan quota
windows (5h / weekly / monthly) as color-coded progress bars with reset
countdowns, pay-as-you-go balances, today/month spends, and the runtime
scheduling state (eligible / quota exhausted).

Cards can be dragged to any position (grip ⠿ on each card; drop beside a
card, or on the grid's empty area to send one to the end). The custom order
persists per browser in localStorage (`dsh.nyroUsage.cardOrder.v1`); new
providers append after the ordered ones, and **Reset order** in the toolbar
restores the gateway's natural order.

Data flows browser → same-origin dsh webserver routes → the nyro Admin API,
so the nyro base URL needs no CORS openings and the admin token never
leaves the host process.

## Configure

Settings → Plugins → **Nyro Usage**:

| Field | Meaning |
|---|---|
| `baseUrl` | nyro admin-plane address, e.g. `http://192.168.31.2:19531` (a trailing `/api/v1` is tolerated) |
| `adminToken` | nyro's `NYRO_ADMIN_TOKEN` (Bearer auth); secret — redacted on read-back |
| `refreshSeconds` | panel auto-refresh interval (default 300, min 15) |
| `cacheSeconds` | host-side cache TTL for the bulk usage call (default 30; nyro queries its upstreams live on every call, so the cache keeps frequent refreshes polite; 0 disables) |

The card also carries a **Test connection** button that exercises the saved
configuration through the host proxy.

## Install (dsh standard)

Local package (link):

```bash
dsh plugin --profile web add link:/home/ubuntu/code/dsh-nyro-usage
```

or with the super-injector's hot assembly (same durable state, no restart):

```
dev_install_package(dir="/home/ubuntu/code/dsh-nyro-usage", profile="web")
```

Build first: `npm install && npm run build`.

## Routes

All loopback-only (LAN-exposed dsh web deployments do not serve them):

- `GET /api/nyro-usage/usage?refresh=1` — bulk provider usage (TTL-cached)
- `GET /api/nyro-usage/status` — sanitized config + cache age
- `POST /api/nyro-usage/test` — connectivity + auth test

## Requirements

- nyro ≥ the version shipping `GET /api/v1/providers/usage`
- dsh web profile with `@linxin666/dsh-client-ui-web-ui-settings`
  (optional; the settings card falls back to the official settings scope)

## License

Apache-2.0. The settings-card chrome and staged form are inlined from the
dsh-web-ui family shared slice (Apache-2.0).
