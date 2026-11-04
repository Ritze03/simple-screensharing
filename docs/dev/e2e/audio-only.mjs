// Task I10, AUDIO: audio-only shares and the Listen tile through the real UI, headless Chromium. THIS RUN PLAYS AUDIO (a tone, an AudioContext, Opus):
// never run it on the host (superdoc/claude-instructions/testing-and-verification.md "No audio tests on the host"), only inside a test VM
// ("Audio tests in the VMs": arch-ssd / fedora-ssd, with `client/scripts/vm-audio/fakehw.sh` and an `emu_src.py` tone source running).
//
//   node docs/dev/e2e/audio-only.mjs <relay host:port> <certhash> --sender <audio-only-sender binary> --source <PipeWire source name>
//
// The sender is `audio-only-sender` (a throwaway package, outside the repo's client/: it joins a room and does what the bar's audio button does,
// `AppState::start_audio_share(room, None, ..)`, `share` mode; `listen` mode is a native viewer that Listens to the first audio-only sender).
//   A. native audio-only sender -> browser: an audio-only tile with Listen (no video tile), Listen plays (packets decoded), Stop listening.
//   B. browser audio-only share (`#btn-audio` with a stubbed getDisplayMedia: the harness tone + a silent pattern, only the audio is kept) -> a second browser.
//   C. the same browser share -> the native viewer (`listen` mode): the sender has no video, packets arrive.
//   D. a sender with only a webcam and audio: the audio is its own tile, watching the webcam subscribes no audio.
import { spawn } from "node:child_process";
import { createWriteStream, mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const args = process.argv.slice(2);
const opt = (n) => (args.includes(n) ? args[args.indexOf(n) + 1] : null);
const [RELAY, HASH] = args.filter((a, i) => !a.startsWith("--") && !(i > 0 && args[i - 1].startsWith("--")));
const SENDER = opt("--sender");
const SOURCE = opt("--source") || "Floorp";
if (!RELAY || !HASH || !SENDER) throw new Error("usage: audio-only.mjs <relay host:port> <certhash> --sender <bin> [--source <name>]");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
const sleep = L.sleep;
let failed = 0;
const check = (c, what, extra = "") => {
  console.log(`${c ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!c) failed++;
};
const section = (s) => console.log(`\n== ${s}`);

const site = await L.serveSite(0);
const flags = ["--use-fake-device-for-media-stream", "--autoplay-policy=no-user-gesture-required"];
const open = async (name) => {
  const b = await L.openApp(site.url, { name, flags });
  await b.conn.send("Browser.grantPermissions", { permissions: ["videoCapture"], origin: new URL(site.url).origin }).catch(() => {});
  return b;
};
const procs = [];
function spawnSender(mode, link, ...rest) {
  const log = createWriteStream(OUT + `sender-${mode}-${procs.length}.log`);
  const p = spawn(SENDER, [mode, link, ...rest], { stdio: ["ignore", "pipe", "pipe"] });
  const lines = [];
  for (const s of [p.stdout, p.stderr]) s.on("data", (d) => { log.write(d); lines.push(...String(d).split("\n").filter(Boolean)); });
  procs.push(p);
  return { p, lines, exited: new Promise((r) => p.on("exit", r)) };
}
const watchOf = (X) => X.page.eval(`(() => { const r = __sss.S.rooms[0]; const w = [...r.watches.values()][0]; if (!w) return null; const s = r.client.watchStats(w.key) || {}; return { audioOnly: w.audioOnly, packets: s.packets, decoded: s.decodedFrames, level: s.level, kind: s.kind, audioKey: w.handle.audioKey || null }; })()`);
const listenTile = (X) => X.page.eval(`(() => { const t = document.querySelector('.st-tile.audio-only'); return t ? { text: t.innerText, btn: (t.querySelector('[data-listen]') || {}).innerText || null, tag: t.querySelector('.tag') && t.querySelector('.tag').innerText } : null; })()`);
async function listenCheck(X, label) {
  await X.page.click({ js: "document.querySelector('.st-tile.audio-only [data-listen]')" });
  await X.page.waitFor("__sss.S.rooms[0].watches.size === 1", { timeout: 10000, what: "listening" });
  let maxLevel = 0;
  let w = null;
  for (let i = 0; i < 60; i++) {
    await sleep(250);
    w = await watchOf(X);
    if (w && w.level > maxLevel) maxLevel = w.level;
    if (w && w.packets > 100 && w.decoded > 50) break;
  }
  check(w && w.audioOnly && w.kind === "audio" && w.packets > 100 && w.decoded > 50, `${label}: Listen plays the audio-only stream (packets decoded)`, JSON.stringify({ ...w, maxLevel: +maxLevel.toFixed(3) }));
  check(maxLevel > 0.01, `${label}: the meter sees the tone`, maxLevel.toFixed(3));
  return w;
}

try {
  const V = await open("Vera");
  await L.createRoom(V.page, { name: "audio", server: RELAY, hash: HASH });
  await L.waitJoined(V.page);
  const link = await L.inviteLink(V.page);

  section("A. a native audio-only sender (no screen) -> the browser");
  const A = spawnSender("share", link, SOURCE, "25");
  await V.page.waitFor("!!document.querySelector('.st-tile.audio-only')", { timeout: 30000, what: "the audio-only tile" });
  const t = await listenTile(V);
  check(t && /native-audio is sharing audio/.test(t.text) && /Audio only/.test(t.text) && /Listen/.test(t.btn || ""), "an audio-only tile: `native-audio is sharing audio`, `Audio only`, Listen", JSON.stringify(t));
  check(await V.page.eval("__sss.S.rooms[0].streams.filter((s) => !s.self).every((s) => s.kind === 'audio') && document.querySelectorAll('.arena > .st-tile').length === 1"), "the sender has no video stream: the stage has the one audio tile and no video tile");
  await listenCheck(V, "native sender");
  await V.page.screenshot(OUT + "audio_A_listening.png");
  check(/Stop listening/.test((await listenTile(V)).btn || "") && (await V.page.eval("!!document.querySelector('.st-tile.audio-only.audible') || !!document.querySelector('.pv.audible')")), "while listening the button says Stop listening and the person's ring is green");
  await V.page.click({ js: "document.querySelector('.st-tile.audio-only [data-listen]')" });
  await V.page.waitFor("__sss.S.rooms[0].watches.size === 0", { timeout: 5000, what: "stopped listening" });
  check(/Listen/.test((await listenTile(V)).btn || "") && !/Stop/.test((await listenTile(V)).btn || ""), "Stop listening ends the watch and the button says Listen again");
  // the sender stops by itself after its seconds (`stop_share`, StopStream to the relay): the tile goes away (a killed process would only vanish with the connection's idle timeout)
  await V.page.waitFor("!document.querySelector('.st-tile.audio-only')", { timeout: 60000, what: "the tile disappears when the sender stops" });
  await A.exited;
  check(true, "when the native sender stops sharing, its tile goes away", A.lines.filter((l) => /error/i.test(l)).slice(0, 2).join(" | "));

  section("B. a browser audio-only share (the bar's audio button) -> a second browser");
  const S = await open("Sam");
  await L.addRoomByLink(S.page, link, "audio");
  await L.waitJoined(S.page);
  await L.stubCapture(S.page, { width: 640, height: 360, fps: 15, audio: true });
  check(await S.page.eval("!document.querySelector('#btn-audio').disabled && /Share audio/.test(document.querySelector('#btn-audio').title)"), "with no share running the Share audio button is live and offers an audio-only share", await S.page.eval("document.querySelector('#btn-audio').title"));
  await S.page.click("#btn-audio");
  await S.page.waitFor("__sss.S.captures.some((c) => c.kind === 'audio' && c.state === 'live' && [...c.dests.values()].some((d) => d.state === 'live'))", { timeout: 20000, what: "the audio-only share live" });
  check(await S.page.eval("(() => { const c = __sss.S.captures.find((x) => x.kind === 'audio'); return !c.video && !!c.audio && c.stream.getVideoTracks().length === 0 && c.dests.values().next().value.share.audioOnly === true && c.dests.values().next().value.share.videoStreamId === null; })()"), "the video track was stopped and dropped, the share is audio-only (no video stream announced)");
  check(await S.page.eval("document.querySelector('#btn-audio').parentElement.classList.contains('on') && /Stop sharing audio/.test(document.querySelector('#btn-audio').title) && !document.querySelector('.arena > .st-tile.mine')"), "the button is green and now stops it; your own stage shows no tile for it (like native)");
  await V.page.waitFor("!!document.querySelector('.st-tile.audio-only')", { timeout: 30000, what: "Sam's audio tile on Vera's page" });
  check(await V.page.eval("(() => { const ss = __sss.S.rooms[0].streams.filter((s) => s.ownerName === 'Sam'); return ss.length === 1 && ss[0].kind === 'audio'; })()"), "on the wire: one audio stream from Sam and nothing else");
  await listenCheck(V, "browser sender");
  // mute switch in the audio menu
  await S.page.click(".splitb:has(#btn-audio) .caret");
  await S.page.waitFor("document.querySelector('.menu.m-audio')", { what: "audio menu" });
  check(await S.page.eval("/Stop sharing audio/.test(document.querySelector('.menu.m-audio').innerText) && /Mute/.test(document.querySelector('.menu.m-audio').innerText)"), "the audio menu has Mute and Stop sharing audio for the audio-only share");
  await S.page.key("Escape");

  section("C. the same browser share -> a native viewer");
  const N = spawnSender("listen", link, "20");
  await sleep(1500);
  let packets = 0;
  for (let i = 0; i < 80 && packets < 100; i++) {
    await sleep(250);
    const m = [...N.lines].reverse().find((l) => /LISTEN-PACKETS/.test(l));
    packets = m ? Number(m.split(" ")[1]) : 0;
  }
  check(N.lines.some((l) => /LISTEN-FOUND owner_has_video=false/.test(l)), "the native viewer sees the browser's audio stream and its owner has no video", N.lines.filter((l) => /LISTEN/.test(l)).slice(0, 2).join(" | "));
  check(packets >= 100, "…and receives its Opus packets (about 50 a second)", `${packets} packets`);
  N.p.kill("SIGKILL");

  section("D. a webcam and audio only: the audio is its own tile; watching the webcam subscribes no audio");
  await S.page.click("#btn-webcam");
  await S.page.waitFor("__sss.S.captures.some((c) => c.kind === 'webcam' && c.state === 'live')", { timeout: 20000, what: "Sam's webcam" });
  await V.page.waitFor("document.querySelectorAll('.arena > .st-tile').length === 2", { timeout: 30000, what: "webcam + audio tiles on Vera's page" });
  check(await V.page.eval("!!document.querySelector('.st-tile.audio-only') && [...document.querySelectorAll('.st-tile.idle')].some((t) => /Webcam$/.test(t.dataset.id))"), "Sam's webcam is a tile and so is the audio of the sender that has only a webcam");
  await V.page.click({ js: "document.querySelector('.st-tile.audio-only [data-listen]')" }); // Stop listening (still listening since B): only the webcam is watched from here on
  await V.page.waitFor("__sss.S.rooms[0].watches.size === 0", { timeout: 5000, what: "stopped listening" });
  for (let i = 0; i < 6 && !(await V.page.eval("[...__sss.S.rooms[0].watches.values()].some((w) => w.label === 'Webcam')")); i++) {
    await V.page.click({ js: "document.querySelector('.st-tile.idle [data-watch]')" });
    await sleep(700);
  }
  try {
    await V.page.waitFor("[...__sss.S.rooms[0].watches.values()].some((w) => w.label === 'Webcam' && w.firstFrame)", { timeout: 30000, what: "webcam frames" });
  } catch (e) {
    console.log("diagnostics:", JSON.stringify(await V.page.eval("(() => { const r = __sss.S.rooms[0]; return [...r.watches.values()].map((w) => ({ label: w.label, error: w.error, codec: w.codec, first: w.firstFrame, stats: r.client.watchStats(w.key) })); })()")));
    console.log("sender side:", JSON.stringify(await S.page.eval("(() => { const c = __sss.S.captures.find((x) => x.kind === 'webcam'); const d = c && [...c.dests.values()][0]; return c && { state: c.state, dest: d && d.state, stats: d && d.share && d.share.stats(), settings: c.video.getSettings() }; })()")));
    console.log("sender console:", S.consoleErrors.join(" | "), "| viewer console:", V.consoleErrors.join(" | "));
    throw e;
  }
  check(await V.page.eval("(() => { const w = [...__sss.S.rooms[0].watches.values()].find((x) => x.label === 'Webcam'); return !w.handle.audioKey && !__sss.S.rooms[0].client.subs.has([...__sss.S.rooms[0].streams].find((s) => s.kind === 'audio').key); })()"), "watching the webcam does not subscribe the sender's audio (a webcam is no video for audio-follows-video)");
  await V.page.screenshot(OUT + "audio_D.png");
  await S.page.click("#btn-audio"); // stop the audio-only share
  await V.page.waitFor("!document.querySelector('.st-tile.audio-only')", { timeout: 20000, what: "audio tile gone after stop" });
  check(true, "stopping the audio-only share removes its tile on the other page");
  check(V.consoleErrors.length === 0 && S.consoleErrors.length === 0, "no console errors", [...V.consoleErrors, ...S.consoleErrors].join(" | "));
  await S.close();
  await V.close();
} catch (e) {
  failed++;
  console.log("FAIL", e.stack || e.message);
} finally {
  for (const p of procs) p.kill("SIGKILL");
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASSED");
process.exit(failed ? 1 : 0);
