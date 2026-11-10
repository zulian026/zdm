#!/usr/bin/env node
// Stream-row screenshots (headless Chrome via the skill's lib/cdp.mjs).
// Serves the project root over local HTTP so the previews resolve
// ../src/styles.css. Captures the new stream rows (downloading HLS in
// preview.html, merged HLS + DASH-no-ffmpeg note in
// preview-completed.html) at 1440 / 768 / 390 into
// .taste-skill/after-stream/. Run: node .taste-skill/capture-stream.mjs
import { createServer } from "node:http";
import { readFileSync, existsSync, mkdirSync } from "node:fs";
import { join, extname } from "node:path";
import { Browser, sleep } from "/home/hatch/workspace/custom-skills/taste-skill/scripts/lib/cdp.mjs";

const ROOT = "/home/hatch/workspace/zdm-tauri";
const OUT = join(ROOT, ".taste-skill/after-stream");
mkdirSync(OUT, { recursive: true });
const MIME = { ".html": "text/html", ".css": "text/css", ".js": "text/javascript", ".png": "image/png", ".woff2": "font/woff2", ".json": "application/json" };

function serve(root, port) {
  return new Promise((resolve) => {
    const s = createServer((req, res) => {
      const p = join(root, decodeURIComponent(req.url.split("?")[0]).replace(/^\/+/, "") || "index.html");
      if (!existsSync(p) || p.includes("..")) { res.writeHead(404); res.end(); return; }
      res.writeHead(200, { "content-type": MIME[extname(p)] || "application/octet-stream" });
      res.end(readFileSync(p));
    });
    s.listen(port, "127.0.0.1", () => resolve(s));
  });
}

const server = await serve(ROOT, 8933);
const browser = await Browser.launch({ width: 1440, height: 900 });

// Clip a screenshot around a row's rect (a little context above and
// below), at the given viewport width. Returns the row's text so the
// caller can assert the stream line rendered.
async function shot(page, url, rowId, width, name, container) {
  await page.goto(url);
  const rect = await page.eval(`(() => {
    const el = document.querySelector('${container} [data-id="${rowId}"]');
    if (!el) return null;
    el.scrollIntoView({ block: "center" });
    const r = el.getBoundingClientRect();
    // CDP clip coordinates are document-relative, but the rect is
    // viewport-relative: below 900px the window scrolls (desktop
    // scrolls #content instead, scrollY 0 there).
    return { x: r.x, y: r.y + window.scrollY, width: r.width, height: r.height };
  })()`);
  if (!rect) {
    console.log(`FAIL  row ${rowId} not found at ${width}px (${name})`);
    return null;
  }
  await sleep(300);
  const text = await page.eval(`document.querySelector('${container} [data-id="${rowId}"]')?.innerText || ""`);
  await page.screenshot({ path: join(OUT, name), clip: { x: 0, y: Math.max(0, rect.y - 80), width, height: Math.min(900, rect.height + 160) } });
  console.log(`PASS  ${name} — row text: ${JSON.stringify(text.replace(/\n/g, " | ").slice(0, 200))}`);
  return text;
}

const page = await browser.newPage();
for (const width of [1440, 768, 390]) {
  await page.setViewport({ width, height: 900 });
  await shot(page, "http://127.0.0.1:8933/.taste-skill/preview.html", "d13", width, `stream-downloading-${width}.png`, "#download-list");
  await shot(page, "http://127.0.0.1:8933/.taste-skill/preview-completed.html", "d13", width, `stream-merged-${width}.png`, "#completed-list");
  await shot(page, "http://127.0.0.1:8933/.taste-skill/preview-completed.html", "d14", width, `stream-dash-note-${width}.png`, "#completed-list");
}
await browser.close();
server.close();
console.log("done — .taste-skill/after-stream/");
