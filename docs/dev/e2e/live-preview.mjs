// Task #78 through the REAL site UI: the sharer's own tile shows a live preview of the local capture while the page is
// visible AND focused, and detaches it (srcObject = null, element hidden) when not. Headless Chromium reports
// document.hasFocus() === false by default, so the run starts with CDP Emulation.setFocusEmulationEnabled; a blur is
// simulated (hasFocus overridden + a `blur` event), hiding is real: a second tab is activated (the first becomes
// visibilityState "hidden") and brought back with Page.bringToFront.
//
//   usage: node docs/dev/e2e/live-preview.mjs <relay host:port> <certhash>   (relay: docs/dev/relay.sh start ..., or the rig's)
//   env:   E2E_OUT = where the screenshots go (default /tmp/sss-e2e), SHOT_PREFIX (default "t78_browser_")
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const [RELAY, HASH] = process.argv.slice(2);
if (!RELAY || !HASH) throw new Error("usage: live-preview.mjs <relay host:port> <certhash>");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
const PFX = process.env.SHOT_PREFIX || "t78_browser_";
mkdirSync(OUT, { recursive: true });

let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};

const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "client-1" });
const probe = () =>
  A.page.eval(`(() => {
    const v = document.querySelector(".thumb.mine > video.live-preview");
    if (!v) return null;
    return { hidden: v.hidden, attached: !!v.srcObject, tracks: v.srcObject ? v.srcObject.getTracks().map((t) => t.kind) : [],
      w: v.videoWidth, t: v.currentTime, paused: v.paused, chip: getComputedStyle(document.querySelector(".thumb.mine .pv")).display,
      label: getComputedStyle(document.querySelector(".thumb.mine .mine-label")).display,
      vis: document.visibilityState, focus: document.hasFocus() };
  })()`);
try {
  await A.page.send("Emulation.setFocusEmulationEnabled", { enabled: true });
  await L.createRoom(A.page, { name: "preview", server: RELAY, hash: HASH });
  await L.waitJoined(A.page);
  await L.stubCapture(A.page, { audio: false });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  await L.sleep(1200);

  // focused + visible: preview plays in the own tile, video only
  let p = await probe();
  check(p && !p.hidden && p.attached && p.w > 0, "focused: the own tile shows the live preview", JSON.stringify(p));
  check(p && p.tracks.join() === "video", "preview stream is video only", p && p.tracks.join());
  check(p && p.chip !== "none" && p.label === "none", "PREVIEW chip shown, placeholder label hidden");
  await A.page.screenshot(OUT + PFX + "focused_1.png");
  const t1 = p && p.t;
  await L.sleep(1000);
  p = await probe();
  check(p && p.t > t1 + 0.5, "the preview keeps playing (currentTime advances)", `${t1} -> ${p && p.t}`);
  await A.page.screenshot(OUT + PFX + "focused_2.png");

  // survives a re-render (the UI rebuilds the tree on every state change); same element, still playing
  await A.page.eval(`window.__pvEl = document.querySelector(".thumb.mine > video.live-preview")`);
  await A.page.eval(`__sss.render()`);
  await L.sleep(500);
  p = await probe();
  check(p && !p.hidden && p.attached && (await A.page.eval(`window.__pvEl === document.querySelector(".thumb.mine > video.live-preview")`)), "survives render(): same element, still attached");

  // window loses focus (another app in front, page still visible) -> detached and hidden. Simulated: CDP focus
  // emulation cannot force hasFocus() to false once the page was clicked, so the page's hasFocus is overridden and
  // the `blur` event the browser would fire is dispatched.
  await A.page.eval(`Object.defineProperty(document, "hasFocus", { value: () => false, configurable: true }); window.dispatchEvent(new Event("blur"))`);
  await L.sleep(500);
  p = await probe();
  check(p && p.hidden && !p.attached && p.chip === "none" && p.label !== "none", "blur: preview detached, hidden, placeholder back", JSON.stringify(p));
  await A.page.screenshot(OUT + PFX + "unfocused.png");
  // a re-render while unfocused must not bring it back
  await A.page.eval(`__sss.render()`);
  await L.sleep(300);
  p = await probe();
  check(p && p.hidden && !p.attached, "blur + render(): still detached");

  // focus again -> resumes
  await A.page.eval(`delete document.hasFocus; window.dispatchEvent(new Event("focus"))`);
  await L.sleep(800);
  p = await probe();
  check(p && !p.hidden && p.attached && p.w > 0, "focus: preview resumes", JSON.stringify(p));

  // tab hidden for real (another tab activated) -> detached; brought back -> resumes
  // (focus emulation keeps a page "visible" even behind another tab, so it is switched off for this part)
  await A.page.send("Emulation.setFocusEmulationEnabled", { enabled: false });
  const { targetId } = await A.page.send("Target.createTarget", { url: "about:blank" });
  await A.page.send("Target.activateTarget", { targetId });
  await A.page.waitFor(`document.visibilityState === "hidden"`, { what: "page hidden by the second tab" });
  await L.sleep(300);
  p = await probe();
  check(p && p.vis === "hidden" && p.hidden && !p.attached, "tab hidden: preview detached", JSON.stringify(p));
  await A.page.send("Page.bringToFront");
  await A.page.waitFor(`document.visibilityState === "visible"`, { what: "page visible again" });
  await L.sleep(800);
  p = await probe();
  check(p && p.vis === "visible" && !p.hidden && p.attached, "tab visible again: preview resumes", JSON.stringify(p));

  // the share itself is unaffected: still live
  check(await A.page.eval(`__sss.S.captures.some((c) => c.state === "live" && [...c.dests.values()].every((d) => d.state === "live"))`), "the share stayed live throughout");
  const errs = A.consoleErrors;
  check(errs.length === 0, "no console errors", errs.join(" | "));
} catch (e) {
  failed++;
  console.log("FAIL", e.message);
  await A.page.screenshot(OUT + PFX + "fail.png").catch(() => {});
} finally {
  await A.close();
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASSED");
process.exit(failed ? 1 : 0);
