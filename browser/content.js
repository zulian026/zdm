// ZDM Browser Integration — page media grabber (content script).
//
// This file is the shared source of truth. Identical copies live at
// browser/chromium/content.js and browser/firefox/content.js
// (an extension can only load files inside its own directory).
//
// How it works:
//  - Scans the live DOM for <video>/<audio> and reads `currentSrc`,
//    which catches sources set by JavaScript that never appear in the
//    static HTML. A MutationObserver + `loadedmetadata` listener
//    re-scans as players inject or change sources.
//  - A small "Download with ZDM" button appears on hover near the
//    top-right corner of a video (IDM-style). Clicking it sends the
//    URL to background.js, which forwards it to the ZDM native host
//    ("com.zdm.native") — the same path as the context menu, so the
//    ZDM confirmation popup applies as usual.
//  - Right-click a video/audio element -> "Download video/audio with
//    ZDM" (context menu, in background.js) covers players where the
//    floating button is awkward.
//
// Limits, stated honestly:
//  - blob: and data: sources are skipped — the browser does not hand
//    those bytes out to extensions, so there is nothing to download.
//  - A .m3u8/.mpd URL goes through ZDM's stream merging (HLS to
//    .mp4, DASH to .mp4 via ffmpeg; live streams rejected, DASH
//    without ffmpeg falls back to the playlist file).
//  - Players that fetch media purely over XHR/fetch (no <video> src)
//    are not seen by this script.
"use strict";

const ext = globalThis.chrome;

// One <style> block for the button; injected once per page.
const STYLE_ID = "zdm-grab-style";
const BTN_CLASS = "zdm-grab-btn";

function ensureStyle() {
  if (document.getElementById(STYLE_ID)) return;
  const style = document.createElement("style");
  style.id = STYLE_ID;
  style.textContent = [
    `.${BTN_CLASS}{`,
    "  position:fixed;z-index:2147483647;display:none;",
    "  background:#111;color:#c6f24e;border:1px solid #c6f24e;",
    "  border-radius:6px;padding:5px 9px;",
    "  font:12px/1.2 system-ui,sans-serif;cursor:pointer;",
    "  white-space:nowrap;",
    "}",
    `.${BTN_CLASS}:hover{background:#c6f24e;color:#111;}`,
  ].join("\n");
  (document.head || document.documentElement).appendChild(style);
}

// Plain http(s) media URL of an element, or null. blob:/data: are
// skipped on purpose (see header).
function mediaUrl(el) {
  const u = el.currentSrc || el.getAttribute("src") || "";
  return /^https?:\/\//i.test(u) ? u : null;
}

const buttons = new Map(); // media element -> button element
let hoverEl = null;
let hideTimer = 0;
let visibleBtn = null;

function hideButton() {
  if (visibleBtn) visibleBtn.style.display = "none";
  visibleBtn = null;
  hoverEl = null;
}

function scheduleHide() {
  clearTimeout(hideTimer);
  hideTimer = setTimeout(hideButton, 250);
}

function positionButton(el, btn) {
  const r = el.getBoundingClientRect();
  const inView =
    r.width >= 120 &&
    r.height >= 60 &&
    r.bottom > 0 &&
    r.right > 0 &&
    r.top < window.innerHeight &&
    r.left < window.innerWidth;
  if (!inView) {
    btn.style.display = "none";
    if (visibleBtn === btn) visibleBtn = null;
    return;
  }
  btn.style.top = `${Math.max(8, r.top + 8)}px`;
  btn.style.right = `${Math.max(8, window.innerWidth - r.right + 8)}px`;
  btn.style.display = "block";
  visibleBtn = btn;
}

function showButton(el) {
  const btn = buttons.get(el);
  if (!btn) return;
  if (visibleBtn && visibleBtn !== btn) visibleBtn.style.display = "none";
  hoverEl = el;
  positionButton(el, btn);
}

function ensureButton(el) {
  if (buttons.has(el)) return;
  ensureStyle();
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = BTN_CLASS;
  btn.textContent = "Download with ZDM";
  btn.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const url = mediaUrl(el);
    if (!url) return;
    try {
      ext.runtime.sendMessage({ type: "zdm-download", url });
    } catch (err) {
      console.warn("[ZDM] send failed:", err);
    }
    hideButton();
  });
  btn.addEventListener("mouseenter", () => clearTimeout(hideTimer));
  btn.addEventListener("mouseleave", scheduleHide);
  document.body.appendChild(btn);
  buttons.set(el, btn);
}

function attach(el) {
  if (el.__zdmGrabAttached) return;
  el.__zdmGrabAttached = true;
  el.addEventListener("mouseenter", () => {
    clearTimeout(hideTimer);
    showButton(el);
  });
  el.addEventListener("mouseleave", scheduleHide);
  el.addEventListener("emptied", () => {
    if (visibleBtn === buttons.get(el)) hideButton();
  });
}

function scan() {
  for (const el of document.querySelectorAll("video, audio")) {
    if (!document.contains(el)) continue;
    const url = mediaUrl(el);
    if (!url) {
      const btn = buttons.get(el);
      if (btn) {
        btn.remove();
        buttons.delete(el);
      }
      continue;
    }
    ensureButton(el);
    attach(el);
  }
  // Drop buttons whose element left the page.
  for (const [el, btn] of buttons) {
    if (!document.contains(el)) {
      btn.remove();
      buttons.delete(el);
      if (visibleBtn === btn) visibleBtn = null;
    }
  }
}

let scanQueued = false;
function queueScan() {
  if (scanQueued) return;
  scanQueued = true;
  setTimeout(() => {
    scanQueued = false;
    scan();
  }, 200);
}

scan();
new MutationObserver(queueScan).observe(document.documentElement, {
  childList: true,
  subtree: true,
  attributes: true,
  attributeFilter: ["src"],
});
document.addEventListener("loadedmetadata", queueScan, true);
window.addEventListener(
  "scroll",
  () => {
    if (hoverEl) positionButton(hoverEl, buttons.get(hoverEl));
  },
  true
);
window.addEventListener("resize", () => {
  if (hoverEl) positionButton(hoverEl, buttons.get(hoverEl));
});
