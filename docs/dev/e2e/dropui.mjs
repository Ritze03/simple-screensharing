// Drop watch through the REAL site UI (task 146): join a room by link, press Watch, then for N seconds write down every session close
// (state + reason), watch-ended, stream vanishing and every notification sound the UI would play (SoundOut.play is replaced by a spy:
// nothing reaches the speakers). The count of "stop" sounds + "Waiting for the first frame" tiles is what the user heard on the phone.
//   node docs/dev/e2e/dropui.mjs '<join link>' [seconds=120] [--phone]
import { writeFileSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const link = process.argv[2];
const secs = Number(process.argv[3] || 120);
const phone = process.argv.includes("--phone");
if (!link) throw new Error("usage: dropui.mjs <join link> [seconds] [--phone]");
const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Phone", viewport: phone ? [390, 844] : undefined });
const log = [];
const out = (o) => { const e = { t: +((Date.now() - t0) / 1000).toFixed(1), ...o }; log.push(e); console.log(JSON.stringify(e)); };
let t0 = Date.now();
try {
  await L.addRoomByLink(A.page, link, "drop");
  await L.waitJoined(A.page);
  await A.page.eval(`(() => {
    window.__played = [];
    __sss.SoundOut.play = async (n) => { window.__played.push([Date.now(), n]); };
    window.__ev = [];
    const r = __sss.S.rooms[0];
    for (const t of ["state", "watch-ended", "error"]) r.client.addEventListener(t, (e) => { const d = { ...e.detail }; delete d.error; window.__ev.push({ at: Date.now(), type: t, ...d, err: e.detail.error && String(e.detail.error.message || e.detail.error) }); });
  })()`);
  t0 = Date.now();
  await L.watchFirst(A.page);
  out({ type: "watching" });
  let seenEv = 0, seenSnd = 0, lastShown = 0, lastWaiting = false;
  const end = Date.now() + secs * 1000;
  while (Date.now() < end) {
    await L.sleep(1000);
    const s = await A.page.eval(`(() => { const r = __sss.S.rooms[0]; const w = [...r.watches.values()][0];
      const st = w && r.client.watchStats(w.key);
      return { state: r.state, ev: window.__ev.slice(${seenEv}), snd: window.__played.slice(${seenSnd}), watching: r.watches.size, dec: st ? st.decoded : null, rxFps: st ? st.receivedFps : null, waiting: /Waiting for the first frame/.test(document.body.innerText), toast: [...document.querySelectorAll('.toast')].map((x) => x.innerText).join('|') }; })()`);
    seenEv += s.ev.length; seenSnd += s.snd.length;
    for (const e of s.ev) out({ ...e, type: "ev:" + e.type, at: undefined, atT: +((e.at - t0) / 1000).toFixed(1) });
    for (const [at, n] of s.snd) out({ type: "SOUND", n, atT: +((at - t0) / 1000).toFixed(1) });
    if (s.waiting !== lastWaiting) out({ type: "waiting-tile", on: s.waiting }), (lastWaiting = s.waiting);
    if (s.state !== "joined") out({ type: "room-state", state: s.state });
  }
  const sum = { sounds: log.filter((e) => e.type === "SOUND").length, closes: log.filter((e) => e.type === "ev:state" && e.state === "reconnecting").length, watchEnded: log.filter((e) => e.type === "ev:watch-ended").length };
  out({ type: "summary", ...sum });
} catch (e) {
  out({ type: "FAIL", err: String(e.message || e) });
}
writeFileSync((process.env.E2E_OUT || ".") + "/dropui.json", JSON.stringify(log));
await A.close?.();
site.close();
process.exit(0);
