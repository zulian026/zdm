#!/usr/bin/env python3
"""Generate a ZDM update manifest (updates/version.json) for a release asset.

The manifest is what ZDM's update checker fetches: version, download
URL, SHA-256 of the asset, and a changelog. The SHA-256 is computed
from the asset file itself, so a tampered download fails exactly like
a checksum mismatch (file kept, never installed).

Usage:
    python3 updates/make-manifest.py \
        --version 0.2.0 \
        --asset release/ZDM_0.2.0_amd64.AppImage \
        --url https://example.com/zdm/releases/0.2.0/ZDM_0.2.0_amd64.AppImage \
        --changelog "Faster downloads." \
        [--out updates/version.json]

Publishing: host the manifest (and the asset) anywhere ZDM can reach
over HTTPS — a GitHub release asset URL works, so does any static
file host. Then paste the manifest URL into ZDM's Settings ("Update
manifest URL"). ZDM checks it once a day at startup and offers the
update when the manifest version is newer than the running one.
Replacing the running binary is still manual: ZDM downloads the new
file and tells you where it is.
"""

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

VERSION_RE = re.compile(r"^v?\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$")


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--version", required=True, help="release version, e.g. 0.2.0")
    p.add_argument("--asset", required=True, help="path to the release asset file")
    p.add_argument("--url", required=True, help="public download URL of the asset")
    p.add_argument("--changelog", default="", help="short changelog for the offer banner")
    p.add_argument(
        "--out",
        default=str(Path(__file__).with_name("version.json")),
        help="where to write the manifest (default: updates/version.json)",
    )
    args = p.parse_args()

    if not VERSION_RE.match(args.version):
        print(f"bad version (want e.g. 0.2.0): {args.version}", file=sys.stderr)
        return 2
    asset = Path(args.asset)
    if not asset.is_file():
        print(f"asset not found: {asset}", file=sys.stderr)
        return 2
    if not (args.url.startswith("http://") or args.url.startswith("https://")):
        print(f"download URL must be http(s): {args.url}", file=sys.stderr)
        return 2

    manifest = {
        "version": args.version.lstrip("v"),
        "download_url": args.url,
        "sha256": sha256_of(asset),
        "changelog": args.changelog,
    }
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"wrote {out} (version {manifest['version']}, sha256 {manifest['sha256'][:12]}…)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
