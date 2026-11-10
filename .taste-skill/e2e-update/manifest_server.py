#!/usr/bin/env python3
"""Tiny update-manifest server for the ZDM update-checker laptop e2e.

Serves GET /version.json (the manifest) and GET /asset.bin (the
release asset). The manifest is generated from the arguments so each
e2e phase can present a different scenario:

  --version 0.2.0        manifest version (default 0.2.0)
  --sha correct|wrong|bad|none
                         correct = sha256 of the asset bytes (default)
                         wrong   = sha256 of different bytes
                         bad     = 64 chars that are not hex
                         none    = omit the sha256 field
  --manifest-body RAW    serve this exact body instead of a generated
                         manifest (e.g. "not json{" for the broken
                         manifest phase)
  --asset-size N         asset is N deterministic bytes (default 65536)
  --port 0               0 = ephemeral; the chosen port is printed as
                         PORT=<n> on stdout (and flushed) so the
                         caller can build the manifest URL.

Every request is logged to stderr (method, path, status).
"""

import argparse
import hashlib
import http.server
import json
import sys


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def build_manifest(args, base: str, asset: bytes) -> str:
    if args.manifest_body is not None:
        return args.manifest_body
    sha = args.sha
    if sha == "correct":
        sha_value = sha256_hex(asset)
    elif sha == "wrong":
        sha_value = sha256_hex(b"tampered asset, not the real bytes")
    elif sha == "bad":
        sha_value = "z" * 64  # 64 chars, not hex
    elif sha == "none":
        sha_value = None
    else:
        raise ValueError(f"unknown --sha: {sha}")
    manifest = {
        "version": args.version,
        "download_url": f"{base}asset.bin",
        "changelog": "Faster downloads. Segment retry is now smarter on flaky connections.",
    }
    if sha_value is not None:
        manifest["sha256"] = sha_value
    return json.dumps(manifest)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--port", type=int, default=0)
    p.add_argument("--version", default="0.2.0")
    p.add_argument("--sha", default="correct",
                   choices=["correct", "wrong", "bad", "none"])
    p.add_argument("--manifest-body", default=None)
    p.add_argument("--asset-size", type=int, default=65536)
    args = p.parse_args()

    asset = bytes((i % 251) for i in range(args.asset_size))

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, fmt, *argv):  # keep stdout clean for PORT=
            sys.stderr.write(f"manifest-server: {fmt % argv}\n")

        def _send(self, status: int, ctype: str, body: bytes):
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/version.json":
                base = f"http://127.0.0.1:{self.server.server_address[1]}/"
                body = build_manifest(args, base, asset).encode()
                self._send(200, "application/json", body)
            elif self.path == "/asset.bin":
                self._send(200, "application/octet-stream", asset)
            else:
                self._send(404, "text/plain", b"not found")

    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"PORT={server.server_address[1]}", flush=True)
    print(f"manifest-server: serving version={args.version} sha={args.sha}"
          f" asset={len(asset)} bytes", file=sys.stderr, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
