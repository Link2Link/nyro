#!/usr/bin/env bash
# Run on the target host. No service installation, credentials or public listener.
set -euo pipefail
binary=$(readlink -f "${1:?Usage: smoke.sh /absolute/path/to/nyro-server}")
[[ -x "$binary" ]] || { echo "Not executable: $binary" >&2; exit 1; }
for tool in curl ss mktemp timeout ps; do
    command -v "$tool" >/dev/null || { echo "Missing tool: $tool" >&2; exit 1; }
done
# Do not probe another instance if these ports are already occupied.
listeners=$(ss -ltn)
if awk '$4 ~ /:(29530|29531)$/ {found=1} END {exit !found}' <<< "$listeners"; then
    echo 'Smoke ports 29530/29531 are already in use' >&2
    exit 1
fi
scratch=$(mktemp -d /tmp/nyro-centos7-smoke.XXXXXX)
pid=
cleanup() {
    if [[ -n "$pid" ]]; then
        kill -TERM "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    printf 'Disposable data and log retained at: %s\n' "$scratch"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
"$binary" --version
# timeout bounds the entire server lifetime even if a probe or SSH session fails.
env -i HOME="$scratch" PATH=/usr/bin:/bin \
    timeout --signal=TERM --kill-after=5 90 "$binary" \
    --mode all --storage-backend sqlite --data-dir "$scratch/data" \
    --proxy-host 127.0.0.1 --proxy-port 29530 \
    --admin-host 127.0.0.1 --admin-port 29531 \
    > "$scratch/server.log" 2>&1 &
pid=$!
ready=false
for ((attempt=0; attempt<60; attempt++)); do
    kill -0 "$pid" 2>/dev/null || { echo 'Server exited before readiness' >&2; exit 1; }
    if curl -q --noproxy '*' -fsS --max-time 1 http://127.0.0.1:29530/readyz > /dev/null 2>&1 \
        && curl -q --noproxy '*' -fsS --max-time 1 http://127.0.0.1:29531/readyz > /dev/null 2>&1; then
        ready=true
        break
    fi
    sleep 0.5
done
[[ $ready == true ]] || { echo 'Readiness timed out' >&2; exit 1; }
# Confirm each listener belongs to this invocation, not a racing instance.
# ss already resolved the owning PID; /proc gives a stable parent relationship
# (old procps on EL7 mis-renders multi-column ps specs).
listeners=$(ss -ltnp)
for port in 29530 29531; do
    owner=$(awk -v pattern="127.0.0.1:$port " 'index($0, pattern) {
        if (match($0, /pid=[0-9]+/)) print substr($0, RSTART + 4, RLENGTH - 4)
    }' <<< "$listeners")
    [[ $owner =~ ^[1-9][0-9]*$ ]] || { echo "Cannot identify owner of listener $port" >&2; exit 1; }
    if [[ $owner != "$pid" ]]; then
        parent=$(awk '{ sub(/.*\) /, ""); print $2 }' "/proc/$owner/stat" 2>/dev/null || true)
        [[ $parent == "$pid" ]] || { echo "Listener $port is not owned by smoke server (pid $owner)" >&2; exit 1; }
    fi
done
for endpoint in 29530/health 29530/readyz 29531/healthz 29531/readyz; do
    body=$(curl -q --noproxy '*' -fsS --max-time 5 "http://127.0.0.1:$endpoint")
    [[ $body == '{"status":"ok"}' ]] || { echo "Unexpected response: $body" >&2; exit 1; }
    printf 'PASS: %s %s\n' "$endpoint" "$body"
done
curl -q --noproxy '*' -fsS --max-time 5 http://127.0.0.1:29531/ -o "$scratch/index.html"
grep -qi '<!doctype html' "$scratch/index.html"
kill -0 "$pid"
echo 'PASS: embedded WebUI, SQLite initialization and loopback HTTP probes'
