#!/usr/bin/env node
// Bento + timeline interaction test (headless Chrome via the skill's
// lib/cdp.mjs). Serves the project root over local HTTP so preview.html
// resolves ../src/styles.css, and dist/ so the app resolves /assets.
// Checks, at 1440 / 768 / 390: bento grid arithmetic (row 1: speed 8/12
// + Today 4/12 share a bottom edge; row 2: timeline panel 12/12), the
// Today panel's figures agreeing with the rows, day groups + rail +
// scrubber jump + flash, scroll containment (only #content scrolls),
// and the phone stack order. Run: node .taste-skill/scroll-test.mjs
import { createServer } from "node:http";
import { readFileSync, existsSync } from "node:fs";
import { join, extname } from "node:path";
import { Browser, sleep } from "/home/hatch/workspace/custom-skills/taste-skill/scripts/lib/cdp.mjs";

const ROOT = "/home/hatch/workspace/zdm-tauri";
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

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok: !!ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? " — " + detail : ""}`);
}
const r = (sel) => document.querySelector(sel)?.getBoundingClientRect().toJSON();

const rootServer = await serve(ROOT, 8931);
const distServer = await serve(join(ROOT, "dist"), 8932);
const browser = await Browser.launch({ width: 1440, height: 900 });

try {
  // ---------- Desktop 1440, populated preview ----------
  const page = await browser.newPage({ width: 1440, height: 900 });
  await page.goto("http://127.0.0.1:8931/.taste-skill/preview.html");
  const m = await page.eval(() => {
    const r = (s) => { const el = document.querySelector(s); return el ? el.getBoundingClientRect().toJSON() : null; };
    const cs = (s, p) => { const el = document.querySelector(s); return el ? getComputedStyle(el, p).backgroundColor : null; };
    const groups = [...document.querySelectorAll("#download-list .tl-group")];
    const cards = [...document.querySelectorAll("#download-list .tl-card")];
    const speed = r(".bento-speed"), today = r(".bento-today"), queue = r(".bento-queue");
    const head = r(".timeline-head"), firstCard = r("#download-list .tl-card");
    const headSize = document.querySelector(".timeline-head .num")?.getBoundingClientRect().toJSON();
    const cardSize = document.querySelector("#download-list .tl-card .dl-col-size")?.getBoundingClientRect().toJSON();
    return {
      titleRight: r(".title-row")?.right,
      speed, today, queue,
      row1BottomsEqual: Math.abs(speed.bottom - today.bottom) < 1.5,
      speedTodayGap: today.left - speed.right,
      queueWidth: queue.width,
      groups: groups.map((g) => g.querySelector(".tl-date")?.textContent),
      groupCounts: groups.map((g) => g.querySelectorAll(".tl-card").length),
      pills: [...document.querySelectorAll("#cat-pills .cat-pill")].map((b) => b.textContent),
      scrub: [...document.querySelectorAll("#scrubber .scrub-btn")].map((b) => b.textContent.trim()),
      rail: cs(".tl-group", "::before"),
      cardBg: cs("#download-list .tl-card"),
      panelBg: cs(".bento-queue"),
      scrubBg: cs(".bento-queue .scrubber"),
      scrubRect: r(".bento-queue .scrubber"),
      headLeft: head?.left, headRight: head?.right,
      cardLeft: firstCard?.left, cardRight: firstCard?.right,
      headSizeLeft: headSize?.left, cardSizeLeft: cardSize?.left,
      headSizeRight: headSize?.right, cardSizeRight: cardSize?.right,
      todayValues: {
        today: document.querySelector("#vit-today")?.textContent,
        active: document.querySelector("#vit-active")?.textContent,
        queued: document.querySelector("#vit-queued")?.textContent,
        queue: document.querySelector("#vit-queue")?.textContent,
      },
      delta: document.querySelector("#speed-delta")?.textContent,
      deltaUp: document.querySelector("#speed-delta")?.classList.contains("delta-up"),
      now: document.querySelector("#speed-now")?.textContent,
    };
  });
  const page_errors = page.consoleErrors;
  check("title row ends at viewport - 22", Math.abs(m.titleRight - 1418) < 1.5, `right=${m.titleRight}`);
  check("row 1: speed 8/12 + Today 4/12 share a bottom edge", m.row1BottomsEqual, `speed.bottom=${m.speed.bottom} today.bottom=${m.today.bottom}`);
  check("row 1: 16px gap between speed and Today", Math.abs(m.speedTodayGap - 16) < 1.5, `gap=${m.speedTodayGap}`);
  check("row 1: speed 775.3 + 16 + Today 379.7 = 1171", Math.abs(m.speed.width - 775.3) < 2 && Math.abs(m.today.width - 379.7) < 2, `speed=${m.speed.width.toFixed(1)} today=${m.today.width.toFixed(1)}`);
  check("row 2: timeline panel spans 1171", Math.abs(m.queueWidth - 1171) < 1.5, `width=${m.queueWidth}`);
  check("timeline panel edges at 247 / 1418", Math.abs(m.queue.left - 247) < 1.5 && Math.abs(m.queue.right - 1418) < 1.5, `left=${m.queue.left} right=${m.queue.right}`);
  check("5 day groups, newest first", JSON.stringify(m.groups) === JSON.stringify(["Today", "Yesterday", "Sat 7 Nov", "Thu 5 Nov", "Fri 30 Oct"]), m.groups.join(", "));
  check("group counts sum to 11", m.groupCounts.reduce((a, b) => a + b, 0) === 11, m.groupCounts.join("+"));
  check("category pills All + 6", m.pills.length === 7 && m.pills[0] === "All", m.pills.join(", "));
  check("scrubber All + 5 days", m.scrub.length === 6 && m.scrub[0] === "All", m.scrub.join(" | "));
  check("rail is #2a2a2a", m.rail === "rgb(42, 42, 42)", m.rail);
  check("cards raised #2a2a2a on #111 panel", m.cardBg === "rgb(42, 42, 42)" && m.panelBg === "rgb(17, 17, 17)", `card=${m.cardBg} panel=${m.panelBg}`);
  check("head/card columns align", Math.abs(m.headSizeLeft - m.cardSizeLeft) < 1.5 && Math.abs(m.headSizeRight - m.cardSizeRight) < 1.5, `head=${m.headSizeLeft}/${m.headSizeRight} card=${m.cardSizeLeft}/${m.cardSizeRight}`);
  check("Today panel agrees with rows", m.todayValues.today.includes("4.2") && m.todayValues.active === "2" && m.todayValues.queued === "1" && m.todayValues.queue === "Downloading", JSON.stringify(m.todayValues));
  check("delta +6.3 vs 5-min average, lime", m.deltaUp && m.delta.includes("+6.3"), m.delta);
  check("scrubber strip bleeds to panel edges", Math.abs(m.scrubRect.left - 247) < 1.5 && Math.abs(m.scrubRect.right - 1418) < 1.5, `left=${m.scrubRect.left} right=${m.scrubRect.right}`);

  // Scroll containment + scrubber jump.
  const before = await page.eval(() => ({
    winY: window.scrollY,
    wm: document.querySelector(".wordmark")?.getBoundingClientRect().toJSON(),
    nav: document.querySelector("#nav")?.getBoundingClientRect().toJSON(),
    settings: [...document.querySelectorAll(".nav-item")].pop()?.getBoundingClientRect().toJSON(),
    top: document.querySelector("#topbar")?.getBoundingClientRect().toJSON(),
  }));
  await page.eval(() => { document.querySelector("main#content").scrollTop = 800; });
  await sleep(300);
  const after = await page.eval(() => ({
    winY: window.scrollY,
    contentTop: document.querySelector("main#content").scrollTop,
    wm: document.querySelector(".wordmark")?.getBoundingClientRect().toJSON(),
    nav: document.querySelector("#nav")?.getBoundingClientRect().toJSON(),
    settings: [...document.querySelectorAll(".nav-item")].pop()?.getBoundingClientRect().toJSON(),
    top: document.querySelector("#topbar")?.getBoundingClientRect().toJSON(),
  }));
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  check("#content scrolls to 800, window stays 0", after.contentTop >= 790 && after.winY === 0, `content=${after.contentTop} win=${after.winY}`);
  check("wordmark/nav/Settings/topbar pinned on scroll", same(before.wm, after.wm) && same(before.nav, after.nav) && same(before.settings, after.settings) && same(before.top, after.top));
  await page.eval(() => { document.querySelector("main#content").scrollTop = 0; });
  await page.eval(() => { [...document.querySelectorAll("#scrubber .scrub-btn")][1].click(); });
  await sleep(900);
  const jump = await page.eval(() => {
    const g = document.querySelector("#download-list .tl-group");
    return { top: g.getBoundingClientRect().top, flash: g.classList.contains("tl-flash"), contentTop: document.querySelector("main#content").scrollTop };
  });
  check("scrubber click scrolls Today group to top with flash", jump.flash && jump.top >= 60 && jump.top < 200, `groupTop=${jump.top.toFixed(0)} flash=${jump.flash} contentTop=${jump.contentTop}`);
  await page.screenshot({ path: join(ROOT, ".taste-skill/after-bento-timeline/desktop-bento.jpg") });
  await page.close();

  // ---------- Completed view ----------
  const cp = await browser.newPage({ width: 1440, height: 900 });
  await cp.goto("http://127.0.0.1:8931/.taste-skill/preview-completed.html");
  const cm = await cp.eval(() => ({
    groups: [...document.querySelectorAll("#completed-list .tl-group")].length,
    verified: [...document.querySelectorAll("#completed-list .verify-ok")].length,
    count: document.querySelector("#completed-count")?.textContent,
    scrub: [...document.querySelectorAll("#completed-scrubber .scrub-btn")].length,
    cardBg: getComputedStyle(document.querySelector("#completed-list .tl-card")).backgroundColor,
  }));
  check("Completed: groups + verified line + count 6 + scrubber 6 buttons", cm.groups === 5 && cm.verified >= 1 && cm.count === "6 completed" && cm.scrub === 6, `groups=${cm.groups} verified=${cm.verified} count=${cm.count} scrub=${cm.scrub}`);
  check("Completed cards stay flat #111 on the ground", cm.cardBg === "rgb(17, 17, 17)", cm.cardBg);
  await cp.close();

  // ---------- Phone 390 ----------
  const mob = await browser.newPage({ width: 390, height: 844 });
  await mob.goto("http://127.0.0.1:8931/.taste-skill/preview.html");
  const mm = await mob.eval(() => {
    const order = ["bento-speed", "bento-today", "bento-queue"].map((c) => document.querySelector("." + c)?.getBoundingClientRect().top);
    return {
      sidebarHidden: getComputedStyle(document.querySelector("#sidebar")).display === "none",
      overflowX: document.documentElement.scrollWidth <= 391,
      railHidden: getComputedStyle(document.querySelector(".tl-group"), "::before").display === "none",
      stacked: order[0] < order[1] && order[1] < order[2],
      order: order.map((t) => Math.round(t)),
    };
  });
  await mob.eval(() => window.scrollTo(0, 500));
  await sleep(300);
  const scrolled = await mob.eval(() => window.scrollY);
  check("phone: sidebar hidden, bento stacks speed > Today > timeline", mm.sidebarHidden && mm.stacked && mm.railHidden, `order=${mm.order} railHidden=${mm.railHidden}`);
  check("phone: page scrolls, no horizontal overflow", scrolled >= 400 && mm.overflowX, `scrollY=${scrolled} overflowX=${mm.overflowX}`);
  await mob.close();

  // ---------- Empty app (dist) ----------
  const app = await browser.newPage({ width: 1440, height: 900 });
  await app.goto("http://127.0.0.1:8932/index.html");
  const am = await app.eval(() => ({
    bento: !!document.querySelector(".bento-speed") && !!document.querySelector(".bento-today") && !!document.querySelector(".bento-queue"),
    today: document.querySelector("#vit-today")?.textContent,
    errors: window.__errors || [],
  }));
  check("empty app: bento renders with Today panel, zeros as zeros", am.bento && am.today === "0 B", `today=${am.today}`);
  check("empty app: no console errors from the new markup", app.consoleErrors.length === 0, app.consoleErrors.join(" | "));
  await app.close();
} finally {
  await browser.close();
  rootServer.close();
  distServer.close();
}

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
if (failed.length) { console.log("FAILED: " + failed.map((f) => f.name).join("; ")); process.exit(1); }
