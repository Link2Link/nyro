#!/usr/bin/env bash
# Reject dependencies not provided by the pinned CentOS 7.6 runtime.
set -euo pipefail
binary=${1:?Usage: verify-binary.sh BINARY}
objdump -f "$binary" | grep -q 'architecture: i386:x86-64'
if objdump -p "$binary" | awk '$1 == "RPATH" || $1 == "RUNPATH" {found=1} END {exit !found}'; then
    echo 'Unexpected RPATH/RUNPATH in distributable binary' >&2
    exit 1
fi
needed=$(objdump -p "$binary" | awk '$1 == "NEEDED" {print $2}')
while IFS= read -r library; do
    case "$library" in
        libc.so.6|libm.so.6|libpthread.so.0|libdl.so.2|librt.so.1|libgcc_s.so.1|ld-linux-x86-64.so.2) ;;
        *) echo "Unexpected runtime dependency: $library" >&2; exit 1 ;;
    esac
done <<< "$needed"
# Old binutils prints versions without parentheses; readelf has a stable Name field.
versions=$(readelf --version-info "$binary" | sed -n 's/.*Name: GLIBC_\([0-9.]*\).*/\1/p' | sort -Vu)
[[ -n "$versions" ]] || { echo 'No GLIBC symbol versions found' >&2; exit 1; }
maximum=$(printf '%s\n' "$versions" | tail -1)
[[ $(printf '%s\n' "$maximum" 2.17 | sort -V | tail -1) == 2.17 ]] || {
    echo "Requires GLIBC_$maximum, newer than target GLIBC_2.17" >&2; exit 1;
}
# Running this gate inside the builder also validates GCC symbol versions.
ldd "$binary"
"$binary" --version
printf 'PASS: x86_64, maximum GLIBC_%s, runtime dependency allowlist\n' "$maximum"
