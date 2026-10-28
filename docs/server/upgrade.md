# In-place updates (nyro-server)

[中文](upgrade_CN.md)

A running `nyro-server` can install a newer release from **Settings → Updates** in the WebUI. Updates only move forward; there is no rollback.

## How it works

1. **Check.** The instance downloads `nyro-server-update.json` from the latest GitHub Release. The file is a manifest of per-platform static binaries (version, notes, sha256, size), signed with minisign. The instance verifies the signature with a public key compiled into the binary (`crates/nyro-core/src/update/release.pub`). Unsigned or tampered manifests are rejected, and the key cannot be changed at runtime.
2. **Install.** After you click **Update now** and confirm, the instance does the following:
   - fetches and verifies the manifest again;
   - downloads the binary for its platform into `<data-dir>/upgrade/staging/`;
   - checks the size and sha256 against the signed manifest;
   - runs `<new binary> --version`, which catches a wrong architecture, a missing libc, or a `noexec` mount;
   - moves the binary to `<data-dir>/upgrade/versions/<version>/nyro-server`;
   - records it as active in `<data-dir>/upgrade/state.json`.
3. **Restart.** Listeners stop accepting new connections. In-flight requests get up to 30 seconds to finish, database pools close, and the process `exec`s the new binary with the same arguments and environment. The PID stays the same, so a container's PID 1 survives. The WebUI waits for the new version to answer and then reloads.
4. **Later starts.** On every start, `nyro-server` reads `state.json` before binding any port. If a newer installed version exists, it hands off to it. If the running binary is the same version or newer (for example, the image was rebuilt with a newer release), it removes the installed upgrades and keeps running.

Database migrations run as part of the new version's normal start.

```text
<data-dir>/upgrade/
  state.json                     {"schema":1,"active":"2.2.0","installed_at":...}
  versions/2.2.0/nyro-server     the active binary (older versions are pruned)
  staging/                       partial downloads
```

## Requirements

- **Unix host** (Linux or macOS). Release assets currently cover `linux-x86_64` and `linux-aarch64` as static musl builds, which run on any Linux libc, CentOS 7 included.
- **A writable data directory** that persists across restarts. Without it, the WebUI shows that in-place updates are unavailable.
- **The admin listener** (`--mode all` or `--mode admin`). Proxy-only replicas are not self-updated.
- **One instance per data directory.** Multi-replica deployments that share storage should set `NYRO_SELF_UPDATE=false` and roll out images instead.
- **Outbound HTTPS to GitHub**, or set **Settings → Proxy** (update traffic uses the gateway proxy), or a mirror via `NYRO_UPDATE_MANIFEST_URL`. Integrity comes from the signature, not the transport, so a mirror cannot alter what is installed.

## Docker

The image's `/var/lib/nyro` volume holds the installed upgrade, so the new version survives `docker restart` and `docker compose up -d` recreation. The binary inside the image acts only as the bootstrapper. In-place updates start working once the image itself contains a release with this feature; older images need one regular image upgrade first.

- `docker inspect` and image labels keep showing the image version. **Settings** shows the version actually running.
- Rebuilding the image with a newer release makes the installed upgrade obsolete, and it is removed on the next start.
- To force the image binary again, stop the container and delete `upgrade/` from the volume.
- Keep a restart policy such as `unless-stopped`. If the process exits after an update, the container comes back on the new version.
- During a restart, the healthcheck may fail once for a few seconds.

## Configuration

| Option | Environment variable | Default | Effect |
|--------|----------------------|---------|--------|
| `--update-check` | `NYRO_UPDATE_CHECK` | `true` | Allow release checks. `false` turns off all outbound update traffic. |
| `--self-update` | `NYRO_SELF_UPDATE` | `true` | Allow installing in place. With `false`, the WebUI still notifies you about new releases. |
| `--update-manifest-url` | `NYRO_UPDATE_MANIFEST_URL` | latest GitHub Release asset | Signed manifest location (mirror). |

Admin API (all routes require the admin token):

| Route | Purpose |
|-------|---------|
| `GET /api/v1/system/version` | Running version, platform, and self-update support (`reason` when unsupported) |
| `GET /api/v1/system/update/check[?force=true]` | Latest signed release. Results are cached for 10 minutes unless forced. |
| `POST /api/v1/system/update/apply` `{"version":"X.Y.Z"}` | Start installing. The version must equal the latest release and be newer than the running one. |
| `GET /api/v1/system/update/status` | Job phase (`downloading`, `verifying`, `installing`, `restarting`, `failed`) and progress |

## Release signing (maintainers)

Every release pushes the static binaries and `nyro-server-update.json` to the GitHub Release, signed by `.github/workflows/release.yml` with the repository secret `NYRO_UPDATE_SIGNING_KEY`.

One-time setup:

```bash
minisign -G -W -p nyro-update.pub -s nyro-update.key   # unencrypted key for CI
cp nyro-update.pub crates/nyro-core/src/update/release.pub
gh secret set NYRO_UPDATE_SIGNING_KEY < nyro-update.key
```

Keep `nyro-update.key` offline. The workflow verifies every signature against `release.pub` and fails the release if they do not match, so a wrong secret cannot publish a manifest that instances would reject. Without the secret, the release still publishes, with a warning, but running instances will not be offered it. While the latest release has no manifest (for example, releases published before self-update existed), **Settings → Updates** reports that no in-place update is published yet instead of an error.

Rotating the key requires shipping a release that contains the new `release.pub`, signed with the **old** key. Instances on older versions only trust the old key.

Test fixtures under `crates/nyro-core/tests/fixtures/update/` are signed with a throwaway key. Regenerate them with `regenerate.sh`.
