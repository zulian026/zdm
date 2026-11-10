#!/usr/bin/env python3
"""Range-capable static file server for the ZDM auto-resume E2E.

Serves files from --dir with real HTTP Range support (single range
only, like the test server in src-tauri/tests/download_integration.rs)
and logs every request to --log:

    <iso-timestamp> <method> <path> Range=<header or -> <status> <bytes-sent>

The log is the resume proof on the laptop: after ZDM is killed and
reopened, the requests for the test file must all carry a Range header
starting at a non-zero offset — a full (unranged) GET after the
restart would mean the download restarted from zero.

Stdlib only. Usage:
    python3 range_server.py --dir <dir> --port 8765 --log <logfile>
"""

import argparse
import datetime
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    directory = "."
    log_path = ""
    dribble = 0.0
    dribble_suffix = ".ts"

    def log_request_line(self, line):
        pass  # keep stdout clean; our own log below is the record

    def log_message(self, fmt, *args):
        pass

    def _log(self, status, sent):
        line = (
            f"{datetime.datetime.now().isoformat(timespec='seconds')} "
            f"{self.command} {self.path} "
            f"Range={self.headers.get('Range', '-')} {status} {sent}\n"
        )
        with threading.Lock():
            with open(self.log_path, "a", encoding="utf-8") as f:
                f.write(line)

    def _serve(self):
        path = self.path.split("?", 1)[0].lstrip("/")
        if not path or ".." in path:
            self.send_response(404)
            self.end_headers()
            self._log(404, 0)
            return
        full = os.path.join(self.directory, path)
        if not os.path.isfile(full):
            self.send_response(404)
            self.end_headers()
            self._log(404, 0)
            return
        size = os.path.getsize(full)
        range_hdr = self.headers.get("Range")
        start, end = 0, size - 1
        status = 200
        if range_hdr and range_hdr.startswith("bytes="):
            try:
                spec = range_hdr[6:].split("-", 1)
                start = int(spec[0]) if spec[0] else 0
                end = int(spec[1]) if spec[1] else size - 1
                end = min(end, size - 1)
                if start > end:
                    raise ValueError("empty range")
                status = 206
            except ValueError:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{size}")
                self.end_headers()
                self._log(416, 0)
                return
        length = end - start + 1
        with open(full, "rb") as f:
            f.seek(start)
            body = f.read(length)
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(length))
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if self.command != "HEAD":
            if self.dribble and path.endswith(self.dribble_suffix):
                # First half now, the rest after the delay: a kill
                # issued in between lands while bytes are in flight.
                import time

                half = length // 2
                self.wfile.write(body[:half])
                self.wfile.flush()
                time.sleep(self.dribble)
                self.wfile.write(body[half:])
            else:
                self.wfile.write(body)
        self._log(status, length if self.command != "HEAD" else 0)

    def do_GET(self):
        self._serve()

    def do_HEAD(self):
        self._serve()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True)
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--log", required=True)
    ap.add_argument("--dribble", type=float, default=0.0,
                    help="delay in seconds between the two halves of a dribbled body (0 = off)")
    ap.add_argument("--dribble-suffix", default=".ts",
                    help="only dribble responses whose path ends with this suffix")
    args = ap.parse_args()
    Handler.directory = os.path.abspath(args.dir)
    Handler.log_path = os.path.abspath(args.log)
    Handler.dribble = args.dribble
    Handler.dribble_suffix = args.dribble_suffix
    open(Handler.log_path, "a", encoding="utf-8").close()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"serving {Handler.directory} on 127.0.0.1:{args.port}, logging to {Handler.log_path}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
