const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const HASH = process.argv[2];
const site = await L.serveSite(0);
console.log("site", site.url);
const A = await L.openApp(site.url, { name: "Alice" });
const B = await L.openApp(site.url, { name: "Bob" });
try {
  const link = await L.createRoom(A.page, { name: "Test", server: "127.0.0.1:14433", hash: HASH });
  console.log("link", link);
  await L.waitJoined(A.page);
  await L.stubCapture(A.page, { fps: 60 });
  await L.goLive(A.page, { codec: "h264", fps: 60 });
  await L.addRoomByLink(B.page, await L.inviteLink(A.page), "Test");
  await L.waitJoined(B.page);
  await L.watchFirst(B.page);
  await L.sleep(3000);
  console.log(JSON.stringify(await L.measureWatch(B.page, 6000)));
  await A.page.screenshot("/tmp/sss-e2e/b2b-A.png");
  await B.page.screenshot("/tmp/sss-e2e/b2b-B.png");
  console.log("A errors", A.consoleErrors, A.badRequests);
  console.log("B errors", B.consoleErrors, B.badRequests);
} catch (e) {
  console.log('FAIL', e.message);
  await B.page.screenshot('/tmp/sss-e2e/b2b-B-fail.png'); await A.page.screenshot('/tmp/sss-e2e/b2b-A-fail.png');
  console.log((await B.page.eval('document.body.innerText')).slice(0,600));
} finally {
  await A.close();
  await B.close();
  site.close();
}
process.exit(0);
