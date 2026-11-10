// ZDM Browser Integration — automatic download interception.
//
// This file is the shared source of truth. Identical copies live at
// browser/chromium/background.js and browser/firefox/background.js
// (an extension can only load files inside its own directory).
//
// How it works:
//  - chrome.downloads.onCreated fires for every download the browser
//    starts. When auto-intercept is ON (default) and the URL is
//    http(s), the browser download is cancelled + erased from the
//    shelf, and the URL is sent to the ZDM native host
//    ("com.zdm.native"), which appends it to ~/Downloads/ZDM/queue.json
//    and launches ZDM if needed. The Tauri app polls that file ~2 s.
//  - Right-click a link -> "Download with ZDM" sends just that link
//    (works even when auto-intercept is OFF). Right-click a video or
//    audio element -> "Download video/audio with ZDM" sends its
//    current source. The floating button from content.js reports
//    through runtime.onMessage below.
//  - Click the toolbar icon to toggle auto-intercept ON/OFF (badge
//    shows the state; it is stored in chrome.storage.local).
//
// `chrome` is used directly (not `browser`): the chrome namespace
// exists in Chromium MV3 and in Firefox MV3, with callback-style
// APIs in both, so one code path works in both browsers.
"use strict";

const HOST_NAME = "com.zdm.native";
const STORAGE_KEY = "zdmIntercept";

const ext = globalThis.chrome;

function getInterceptEnabled() {
  return new Promise((resolve) => {
    try {
      ext.storage.local.get({ [STORAGE_KEY]: true }, (items) => {
        if (ext.runtime.lastError) return resolve(true);
        resolve(items[STORAGE_KEY] !== false);
      });
    } catch (e) {
      resolve(true);
    }
  });
}

function setInterceptEnabled(on) {
  return new Promise((resolve) => {
    try {
      ext.storage.local.set({ [STORAGE_KEY]: on }, () => resolve());
    } catch (e) {
      resolve();
    }
  });
}

function isInterceptable(url) {
  // Only plain web downloads. blob:, data:, chrome://, about:,
  // moz-extension:// and file:// are left to the browser.
  return typeof url === "string" && /^https?:\/\//i.test(url);
}

function sendToZdm(url) {
  ext.runtime.sendNativeMessage(HOST_NAME, { url }, (resp) => {
    if (ext.runtime.lastError) {
      console.warn(
        "[ZDM] native host error (run browser/install.sh?):",
        ext.runtime.lastError.message
      );
      return;
    }
    console.log("[ZDM] queued:", url, resp);
  });
}

async function refreshAction() {
  const on = await getInterceptEnabled();
  try {
    ext.action.setTitle({
      title: on
        ? "ZDM: auto-download ON — click to turn off"
        : "ZDM: auto-download OFF — click to turn on",
    });
    ext.action.setBadgeText({ text: on ? "ON" : "OFF" });
    ext.action.setBadgeBackgroundColor({ color: on ? "#c6f24e" : "#666666" });
  } catch (e) {
    // action API unavailable; interception still works.
  }
}

ext.runtime.onInstalled.addListener(() => {
  ext.contextMenus.create({
    id: "zdm-download",
    title: "Download with ZDM",
    contexts: ["link"],
  });
  ext.contextMenus.create({
    id: "zdm-download-video",
    title: "Download video with ZDM",
    contexts: ["video"],
  });
  ext.contextMenus.create({
    id: "zdm-download-audio",
    title: "Download audio with ZDM",
    contexts: ["audio"],
  });
  refreshAction();
});

ext.runtime.onStartup.addListener(() => refreshAction());

// Automatic interception.
ext.downloads.onCreated.addListener(async (item) => {
  if (!isInterceptable(item.url)) return;
  if (!(await getInterceptEnabled())) return;
  ext.downloads.cancel(item.id, () => {
    if (ext.runtime.lastError) {
      // Already finished (tiny file) or already gone: leave it to
      // the browser instead of downloading it twice.
      console.warn("[ZDM] cancel failed:", ext.runtime.lastError.message);
      return;
    }
    ext.downloads.erase({ id: item.id }, () => {});
    sendToZdm(item.url);
  });
});

// Fallback: right-click a link -> "Download with ZDM".
ext.contextMenus.onClicked.addListener((info) => {
  if (info.menuItemId === "zdm-download" && info.linkUrl) {
    sendToZdm(info.linkUrl);
  }
  // Right-click a video/audio element -> download its current source.
  if (
    (info.menuItemId === "zdm-download-video" ||
      info.menuItemId === "zdm-download-audio") &&
    info.srcUrl &&
    isInterceptable(info.srcUrl)
  ) {
    sendToZdm(info.srcUrl);
  }
});

// Floating button from content.js (page media grabber).
ext.runtime.onMessage.addListener((msg) => {
  if (msg && msg.type === "zdm-download" && isInterceptable(msg.url)) {
    sendToZdm(msg.url);
  }
});

// Toolbar click toggles auto-intercept.
ext.action.onClicked.addListener(async () => {
  const on = await getInterceptEnabled();
  await setInterceptEnabled(!on);
  refreshAction();
});
