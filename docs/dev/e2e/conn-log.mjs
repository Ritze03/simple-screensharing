// Settings > Connection (task 146): every way a watch or the room connection ends writes its reason into the log the user can read on a phone.
//   node docs/dev/e2e/conn-log.mjs <relay host:port> <certhash>
// A shares, B watches. Then: (1) B's relay session is closed from the page with a close code -> "connection to the relay lost ... (close code 7)" and "back after";
// (2) A stops the share -> "... is gone: they stopped it (we were watching it)"; (3) the Settings > Connection tab shows the lines and a Copy button.
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const [server, hash] = [process.argv[2], process.argv[3]];
if (!server || !hash) throw new Error("usage: conn-log.mjs <relay host:port> <certhash>");
const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Anna" });
const B = await L.openApp(site.url, { name: "Ben" });
let ok = true;
const check = (what, cond) => { console.log(cond ? "PASS" : "FAIL", what); if (!cond) ok = false; };
try {
  const link = await L.createRoom(A.page, { name: "Log", server, hash });
  await L.waitJoined(A.page);
  await L.stubCapture(A.page, { fps: 30 });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  await L.addRoomByLink(B.page, link, "Log");
  await L.waitJoined(B.page);
  await L.watchFirst(B.page);
  await L.sleep(2000);
  check("log is empty while nothing dropped", (await B.page.eval("__sss.S.dropLog.filter((e) => e.kind === 'lost' || e.kind === 'stream').length")) === 0);

  await B.page.eval("__sss.S.rooms[0].client.session.wt.close({ closeCode: 7, reason: 'test close' })");
  await B.page.waitFor("__sss.S.dropLog.some((e) => e.kind === 'back')", { timeout: 20000, what: "back after the close" });
  const lines = await B.page.eval("__sss.S.dropLog.map((e) => e.kind + ': ' + e.text)");
  console.log(lines.join("\n"));
  check("the session close is logged with its reason", lines.some((l) => /^lost: .*connection to the relay lost: test close \(close code 7\)/.test(l)));
  check("the way back is logged", lines.some((l) => /^back: .*back after [\d.]+ s/.test(l)));

  await L.watchFirst(B.page).catch(() => {});
  await A.page.eval("__sss.S.captures.forEach((c) => c.stop && c.stop())").catch(() => {});
  await A.page.eval("(() => { const c = __sss.S.rooms[0].client; for (const s of [...c.shares]) c.stopShare(s); })()");
  await B.page.waitFor("__sss.S.dropLog.some((e) => e.kind === 'stream')", { timeout: 20000, what: "stream gone line" });
  const s2 = await B.page.eval("__sss.S.dropLog.filter((e) => e.kind === 'stream').map((e) => e.text)");
  console.log(s2.join("\n"));
  check("a stopped stream is logged with who and why", s2.some((l) => /Anna's video .* is gone: they stopped it/.test(l)));

  // The Settings tab shows it.
  await B.page.setViewport(390, 844);
  await B.page.eval("__sss.S.modal = { kind: 'settings', tab: 'connection' }; __sss.render()");
  await B.page.waitFor("document.querySelector('#conn-log')", { what: "connection tab" });
  const shown = await B.page.eval("document.querySelector('#conn-log').innerText");
  check("the Connection tab lists the lines", /connection to the relay lost/.test(shown) && /is gone/.test(shown));
  check("the Copy button is there", await B.page.eval("!!document.querySelector('#btn-conn-copy')"));
  await B.page.screenshot((process.env.E2E_OUT || ".") + "/conn-log-phone.png");
  console.log("errors", A.consoleErrors, B.consoleErrors);
} catch (e) {
  console.log("FAIL", e.message);
  ok = false;
}
await A.close?.();
await B.close?.();
site.close();
console.log(ok ? "ALL PASS" : "FAILED");
process.exit(ok ? 0 : 1);
