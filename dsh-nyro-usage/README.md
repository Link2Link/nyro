# dsh-nyro-usage

[Nyro](https://github.com/Link2Link/nyro) provider usage panel for DSH Web.

Adds a **Nyro Usage** entry to the dsh web GUI sidebar and renders every
configured nyro provider's upstream usage in one place: coding-plan quota
windows (5h / weekly / monthly) as color-coded progress bars with reset
countdowns, pay-as-you-go balances, today/month spends, and the runtime
scheduling state (eligible / quota exhausted).

Several nyro gateways can be connected at once: each gateway gets its own
collapsible group in the panel with its own refresh button. Gateways are
queried independently, so an unreachable or misconfigured gateway only shows
an error in its own group.

Cards can be dragged to any position (grip ⠿ on each card; drop beside a
card, or on the grid's empty area to send one to the end). The custom order
persists per browser and per gateway in localStorage
(`dsh.nyroUsage.cardOrder.v1` for the primary gateway,
`dsh.nyroUsage.cardOrder.v1:<gateway id>` for the others); new providers
append after the ordered ones, and **Reset order** in a group's header
restores that gateway's natural order.

Data flows browser → same-origin dsh webserver routes → the nyro Admin API,
so the nyro base URL needs no CORS openings and the admin token never
leaves the host process.

## Claude account pools

Claude Pro/Max pools render their 5-hour and weekly quota summaries as **weighted
estimates**, including coverage, stale/unknown state and the next resetting
accounts. Partial or stale data is not displayed as zero, and a reset for part
of a pool is never shown as a reset for every account. These summaries are
informational, not scheduling inputs.

The panel prefers `usage.pool_quota` (anonymous display-only data) and supports
older direct Admin responses via `usage.account_pool.quota`. Forwarded reports
must include `pool_quota`; upgrade the upstream Nyro to produce this field.
Account identities and configuration remain excluded from forwarding. An older
forwarded Claude report without quota now shows a specific missing-quota notice,
not a misleading hidden-money message. Money visibility still follows API-key
permissions and does not control subscription quota visibility.

## Configure

Settings → Plugins → **Nyro Usage**:

| Field | Meaning |
|---|---|
| `enabled` | plugin master switch (default on); when off the panel shows a "turned off" notice and no gateway is queried |
| `name` | display name of the primary gateway (optional; defaults to the address host) |
| `baseUrl` | primary gateway's nyro admin-plane address, e.g. `http://192.168.31.2:19531` (a trailing `/api/v1` is tolerated); leave empty to skip the primary gateway |
| `adminToken` | primary gateway's `NYRO_ADMIN_TOKEN` (Bearer auth); secret — redacted on read-back |
| `instances` | additional nyro gateways, each `{ name, baseUrl, adminToken, enabled }`, listed after the primary one |
| `refreshSeconds` | panel auto-refresh interval (default 300, min 15) |
| `cacheSeconds` | host-side cache TTL for the bulk usage call, per gateway (default 30; nyro queries its upstreams live on every call, so the cache keeps frequent refreshes polite; 0 disables) |

Existing single-gateway configs (just `baseUrl` / `adminToken`) keep working
unchanged as the primary gateway.

The **More nyro gateways** section of the card adds, edits, removes and
disables gateways. Saved tokens are never shown: leave a token field empty to
keep the stored token; a new gateway needs one. The profile config can also be
edited directly:

```yaml
- id: nyro-usage
  name: dsh-nyro-usage
  config:
    baseUrl: http://192.168.31.2:19531
    adminToken: <primary token>
    instances:
      - name: Office
        baseUrl: http://10.0.0.8:19531
        adminToken: <token>
      - name: Backup
        baseUrl: http://10.0.0.9:19531
        adminToken: <token>
        enabled: false
```

Gateway ids are slugs of the display name (`office`, `backup`), suffixed
`-2`, `-3`… on collisions; the primary gateway is always `default`. Renaming a
gateway changes its id, which resets that gateway's custom card order.

The card also carries a **Test connection** button that tests every saved
gateway through the host proxy and lists the result per gateway.

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

- `GET /api/nyro-usage/usage?instance=<id>&refresh=1` — provider usage per
  gateway, `{ instances: [{ id, name, baseUrl, items, cachedAt, error, errorKind }] }`;
  without `instance` every gateway is queried concurrently; `refresh=1`
  bypasses the cache
- `GET /api/nyro-usage/status` — sanitized config: the gateway list, per-gateway
  cache age and token presence per config entry (never a token)
- `POST /api/nyro-usage/test?instance=<id>` — connectivity + auth test; every
  gateway without `instance`

## Develop

```bash
npm install
npm test        # unit tests for host resolution/routes and the gateway-list editor (node --test)
npm run build   # tsc + tsdown → lib/index.js and lib/client.js
```

## Requirements

- nyro ≥ the version shipping `GET /api/v1/providers/usage`
- dsh web profile with `@linxin666/dsh-client-ui-web-ui-settings`
  (optional; the settings card falls back to the official settings scope)

## License

Apache-2.0. The settings-card chrome and staged form are inlined from the
dsh-web-ui family shared slice (Apache-2.0).
