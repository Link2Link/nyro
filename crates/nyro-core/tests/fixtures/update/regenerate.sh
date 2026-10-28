#!/usr/bin/env bash
# Regenerate the signed self-update fixtures with a throwaway key.
# The secret key is discarded; only test.pub is kept. Requires minisign.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(cd -- "$here/../../../../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

minisign -G -W -p "$here/test.pub" -s "$tmp/test.key" -f >/dev/null

assets=()
for platform in linux-x86_64 linux-aarch64 macos-x86_64 macos-aarch64; do
    assets+=(--asset "$platform=nyro-server-test")
done

python3 "$root/scripts/release/gen_server_update.py" \
    --assets-dir "$here" \
    --secret-key "$tmp/test.key" \
    --verify-pubkey "$here/test.pub" \
    --version 9.9.9 \
    --base-url "" \
    --notes "Test release notes" \
    --pub-date 2026-01-01T00:00:00Z \
    "${assets[@]}" \
    --output "$here/nyro-server-update.json"
