#!/usr/bin/env python3
"""Generate the signed self-update manifest for nyro-server releases.

The output `nyro-server-update.json` is an envelope:

    {"manifest": "<manifest JSON text>", "signature": "<minisign signature>"}

The signature covers the exact manifest text. Running instances verify it with
the public key compiled into nyro-core (crates/nyro-core/src/update/release.pub).

Usage (CI):
    VERSION=v2.2.0 REPO=Link2Link/nyro python3 scripts/release/gen_server_update.py \
        --assets-dir release-assets --secret-key /tmp/nyro-update.key \
        --verify-pubkey crates/nyro-core/src/update/release.pub \
        --output release-assets/nyro-server-update.json

The secret key must be an unencrypted minisign key (`minisign -G -W`).
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

# Static (musl) release binaries mapped to manifest platform keys
# (`<std::env::consts::OS>-<std::env::consts::ARCH>`).
PLATFORM_ASSETS = {
    "linux-x86_64": "nyro-server-linux-x86_64-musl",
    "linux-aarch64": "nyro-server-linux-aarch64-musl",
}
SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def changelog_notes(changelog: Path, version: str) -> str:
    """Body of the `## vX.Y.Z` section, without the heading."""
    if not changelog.is_file():
        return ""
    text = changelog.read_text(encoding="utf-8")
    match = re.search(
        rf"^## v?{re.escape(version)}\s*$(.*?)(?=^---\s*$|^## |\Z)",
        text,
        flags=re.MULTILINE | re.DOTALL,
    )
    return match.group(1).strip() if match else ""


def sign(manifest_text: str, secret_key: Path, version: str, minisign: str) -> str:
    with tempfile.TemporaryDirectory() as tmp:
        payload = Path(tmp) / "manifest.json"
        payload.write_bytes(manifest_text.encode("utf-8"))
        signature = Path(tmp) / "manifest.json.minisig"
        subprocess.run(
            [
                minisign, "-S", "-s", str(secret_key), "-m", str(payload),
                "-x", str(signature), "-t", f"nyro-server {version}",
            ],
            check=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
        )
        return signature.read_text(encoding="utf-8")


def verify(manifest_text: str, signature: str, pubkey: Path, minisign: str) -> None:
    """Refuse to publish a manifest that running instances would reject."""
    with tempfile.TemporaryDirectory() as tmp:
        payload = Path(tmp) / "manifest.json"
        payload.write_bytes(manifest_text.encode("utf-8"))
        sig_path = Path(tmp) / "manifest.json.minisig"
        sig_path.write_text(signature, encoding="utf-8")
        result = subprocess.run(
            [minisign, "-V", "-p", str(pubkey), "-m", str(payload), "-x", str(sig_path)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        if result.returncode != 0:
            sys.exit(
                "ERROR: the signing key does not match "
                f"{pubkey}; instances would reject this release.\n{result.stderr.strip()}"
            )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--assets-dir", default="release-assets")
    parser.add_argument("--output", default="nyro-server-update.json")
    parser.add_argument("--secret-key", required=True, type=Path)
    parser.add_argument("--verify-pubkey", type=Path, help="fail unless the signature verifies with this key")
    parser.add_argument("--version", default=os.environ.get("VERSION", ""))
    parser.add_argument("--repo", default=os.environ.get("REPO", ""))
    parser.add_argument(
        "--base-url",
        help="asset URL prefix; default is the GitHub release download URL, "
        "an empty string makes asset URLs relative to the manifest",
    )
    parser.add_argument("--changelog", type=Path, default=Path("CHANGELOG.md"))
    parser.add_argument("--notes", help="release notes; overrides --changelog")
    parser.add_argument("--pub-date", help="RFC 3339 publish time (default: now)")
    parser.add_argument(
        "--asset",
        action="append",
        default=[],
        metavar="PLATFORM=FILE",
        help="explicit platform asset (repeatable); replaces the default musl mapping",
    )
    parser.add_argument("--minisign", default="minisign")
    args = parser.parse_args()

    version = args.version.strip().lstrip("v")
    if not SEMVER.match(version):
        sys.exit(f"ERROR: invalid version `{args.version}` (expected X.Y.Z)")
    tag = f"v{version}"

    if args.base_url is None:
        if not args.repo:
            sys.exit("ERROR: --repo/REPO is required unless --base-url is given")
        base_url = f"https://github.com/{args.repo}/releases/download/{tag}/"
    else:
        base_url = args.base_url
    if base_url and not base_url.endswith("/"):
        base_url += "/"

    if args.asset:
        mapping = {}
        for item in args.asset:
            platform, _, name = item.partition("=")
            if not platform or not name:
                sys.exit(f"ERROR: --asset expects PLATFORM=FILE, got `{item}`")
            mapping[platform] = name
    else:
        mapping = PLATFORM_ASSETS

    assets_dir = Path(args.assets_dir)
    assets = {}
    for platform, name in sorted(mapping.items()):
        path = assets_dir / name
        if not path.is_file():
            print(f"warning: {name} not found; {platform} will not self-update", file=sys.stderr)
            continue
        assets[platform] = {
            "url": f"{base_url}{name}",
            "sha256": sha256_of(path),
            "size": path.stat().st_size,
        }
    if not assets:
        sys.exit(f"ERROR: no self-update binaries found in {assets_dir}")

    notes = args.notes if args.notes is not None else changelog_notes(args.changelog, version)
    pub_date = args.pub_date or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    release_url = (
        f"https://github.com/{args.repo}/releases/tag/{tag}" if args.repo else None
    )
    manifest = {
        "version": version,
        "pub_date": pub_date,
        "notes": notes,
        "release_url": release_url,
        "assets": assets,
    }
    manifest_text = json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True)

    signature = sign(manifest_text, args.secret_key, version, args.minisign)
    if args.verify_pubkey:
        verify(manifest_text, signature, args.verify_pubkey, args.minisign)

    envelope = {"manifest": manifest_text, "signature": signature}
    output = Path(args.output)
    output.write_text(json.dumps(envelope, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {output} for {version}: {', '.join(sorted(assets))}")


if __name__ == "__main__":
    main()
