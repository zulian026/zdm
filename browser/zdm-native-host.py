#!/usr/bin/env python3
"""ZDM native messaging host for the Tauri app.

Chrome/Chromium and Firefox extensions talk to this host via the
Native Messaging protocol (4-byte little-endian length prefix + JSON).
On {"url": ...} it appends the URL to the shared queue file
(~/Downloads/ZDM/queue.json — the same JSON-array-of-strings format
the Python ZDM uses), which the running Tauri app polls every ~2 s,
and launches the Tauri app if it is not already running.

Format of queue.json (shared with Python ZDM, do not change):
    ["https://example.com/a.bin", "https://example.com/b.bin"]

Only the Python standard library is used. The host never crashes on
a bad message: errors are logged to ~/Downloads/ZDM/native-host.log
and answered with {"ok": false, "error": ...}.

Install: run browser/install.sh on the target machine. It registers
this script as the "com.zdm.native" host for Chromium-family
browsers and Firefox.
"""
from __future__ import annotations

import json
import os
import struct
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
DOWNLOAD_DIR = os.path.join(HOME, "Downloads", "ZDM")
QUEUE_FILE = os.path.join(DOWNLOAD_DIR, "queue.json")
LOCK_FILE = os.path.join(DOWNLOAD_DIR, ".queue.lock")
LOG_FILE = os.path.join(DOWNLOAD_DIR, "native-host.log")

# Launch order (first existing + executable wins):
#   1. /usr/bin/zdm — system install from the .deb package
#   2. <project>/release/zdm — standalone binary next to this install
#      (runs without the AppImage's GStreamer appsink warning)
#   3. <project>/release/ZDM_0.1.1_amd64.AppImage
ZDIM_SYSTEM = "/usr/bin/zdm"
PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZDIM_BINARY = os.path.join(PROJECT_DIR, "release", "zdm")
ZDIM_APPIMAGE = os.path.join(PROJECT_DIR, "release", "ZDM_0.1.1_amd64.AppImage")

# A lock older than this is treated as stale (crashed holder).
LOCK_STALE_SECS = 10


def log(msg: str) -> None:
    try:
        os.makedirs(DOWNLOAD_DIR, exist_ok=True)
        with open(LOG_FILE, "a") as f:
            f.write(time.strftime("%Y-%m-%d %H:%M:%S") + " " + msg + "\n")
    except OSError:
        pass


def _read_message():
    raw_len = sys.stdin.buffer.read(4)
    if len(raw_len) < 4:
        return None
    length = struct.unpack("<I", raw_len)[0]
    if length > 1024 * 1024:  # sanity cap: 1 MiB per message
        raise ValueError(f"message too large: {length} bytes")
    data = sys.stdin.buffer.read(length)
    if len(data) < length:
        return None
    return json.loads(data.decode("utf-8"))


def _send_message(obj) -> None:
    data = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(struct.pack("<I", len(data)) + data)
    sys.stdout.buffer.flush()


class _QueueLock:
    """Exclusive-create lock file, shared with the Tauri app's
    drain_queue_file(). Stale locks (>10 s) are removed once."""

    def __init__(self) -> None:
        self.fd = None

    def __enter__(self):
        os.makedirs(DOWNLOAD_DIR, exist_ok=True)
        for _ in range(2):
            try:
                self.fd = os.open(LOCK_FILE, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
                return self
            except FileExistsError:
                try:
                    if time.time() - os.path.getmtime(LOCK_FILE) > LOCK_STALE_SECS:
                        os.unlink(LOCK_FILE)
                        continue
                except OSError:
                    pass
                time.sleep(0.05)
        return self  # could not lock: proceed unlocked (best effort)

    def __exit__(self, *exc):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None
        try:
            os.unlink(LOCK_FILE)
        except OSError:
            pass
        return False


def queue_url(url: str) -> None:
    os.makedirs(DOWNLOAD_DIR, exist_ok=True)
    with _QueueLock():
        urls = []
        if os.path.exists(QUEUE_FILE):
            try:
                with open(QUEUE_FILE) as f:
                    urls = json.load(f)
                if not isinstance(urls, list):
                    urls = []
            except (OSError, ValueError):
                urls = []
        if url not in urls:
            urls.append(url)
        tmp = QUEUE_FILE + ".tmp"
        with open(tmp, "w") as f:
            json.dump(urls, f)
        os.replace(tmp, QUEUE_FILE)


def _zdm_running() -> bool:
    """True if a ZDM binary/AppImage process is alive (pure stdlib:
    scan /proc, no pgrep dependency)."""
    try:
        for pid in os.listdir("/proc"):
            if not pid.isdigit():
                continue
            try:
                with open(f"/proc/{pid}/comm") as f:
                    if f.read().strip() == "zdm":
                        return True
                with open(f"/proc/{pid}/cmdline", "rb") as f:
                    args = f.read().split(b"\0")
                if args and (
                    args[0].endswith(b"/release/zdm")
                    or args[0].endswith(b"/usr/bin/zdm")
                    or b"ZDM_0.1.1_amd64.AppImage" in args[0]
                ):
                    return True
            except (OSError, ValueError):
                continue
    except OSError:
        pass
    return False


def _launch_zdm() -> str | None:
    """Start the Tauri app detached (inherits the browser's DISPLAY).
    Returns the launched path, or None when nothing could be started."""
    target = None
    if os.path.isfile(ZDIM_SYSTEM) and os.access(ZDIM_SYSTEM, os.X_OK):
        target = ZDIM_SYSTEM
    elif os.path.isfile(ZDIM_BINARY) and os.access(ZDIM_BINARY, os.X_OK):
        target = ZDIM_BINARY
    elif os.path.isfile(ZDIM_APPIMAGE) and os.access(ZDIM_APPIMAGE, os.X_OK):
        target = ZDIM_APPIMAGE
    if target is None:
        log(
            f"cannot launch ZDM: no binary at {ZDIM_SYSTEM}, {ZDIM_BINARY} "
            f"nor {ZDIM_APPIMAGE}"
        )
        return None
    try:
        subprocess.Popen(
            [target],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        return target
    except OSError as e:
        log(f"failed to launch ZDM ({target}): {e}")
        return None


def main() -> None:
    while True:
        try:
            msg = _read_message()
        except (ValueError, OSError) as e:
            log(f"bad message framing: {e}")
            break
        if msg is None:
            break
        try:
            url = msg.get("url") if isinstance(msg, dict) else None
            if not url or not isinstance(url, str):
                _send_message({"ok": False, "error": "no url in message"})
                continue
            queue_url(url)
            launched = None
            if not _zdm_running():
                launched = _launch_zdm()
            _send_message({"ok": True, "queued": url, "launched": launched})
        except Exception as e:  # never crash on a bad message
            log(f"error handling message {msg!r}: {e}")
            try:
                _send_message({"ok": False, "error": str(e)})
            except Exception:
                break


if __name__ == "__main__":
    main()
