// Task #69 through the REAL site UI: the "Choose your name" modal (default name `user-xxxx`, Later, Save, never again),
// the room name in the join link (copy-invite carries it, a pasted 5-field link pre-fills the join dialog, the stored
// room keeps the plain link). Headless Chromium, fresh profile.
//
//   usage: node docs/dev/e2e/name-modal.mjs <relay host:port> <certhash>   (relay: docs/dev/relay.sh start ...)
//   env:   E2E_OUT = where the screenshots go (default /tmp/sss-e2e), SHOT_PREFIX (default "t69_")
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const [RELAY, HASH] = process.argv.slice(2);
if (!RELAY || !HASH) throw new Error("usage: name-modal.mjs <relay host:port> <certhash>");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
const PFX = process.env.SHOT_PREFIX || "t69_";
mkdirSync(OUT, { recursive: true });

let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};

const site = await L.serveSite(0);
// name "" = what a fresh profile has: no name chosen
const A = await L.openApp(site.url, { name: "" });
const { page } = A;
const modalState = () => page.eval(`(() => { const i = document.querySelector("#choose-name-in"); return i ? { value: i.value, focused: document.activeElement === i, selected: i.selectionStart === 0 && i.selectionEnd === i.value.length && i.value.length > 0, saveDisabled: document.querySelector("#btn-choose-name-save").disabled, width: document.querySelector(".modal").getBoundingClientRect().width } : null; })()`);
const stored = () => page.eval(`JSON.parse(localStorage.getItem("sss.settings.v1") || "{}")`);
try {
  // 1. fresh profile: the modal is open with the automatic default prefilled, focused and selected
  await page.waitFor("document.querySelector('#choose-name-in')", { what: "choose-name modal" });
  const m1 = await modalState();
  const auto = m1.value;
  check(/^user-[a-z0-9]{4}$/.test(auto), "default name is user-xxxx", auto);
  check(m1.focused && m1.selected, "field focused with its text selected");
  check(!m1.saveDisabled, "Save enabled for the prefilled default");
  check(m1.width === 520, "smallest modal width", String(m1.width));
  await page.screenshot(OUT + PFX + "choose-name-modal.png");

  // 2. Save is disabled for an empty field
  await page.type("#choose-name-in", "   ");
  check((await modalState()).saveDisabled, "Save disabled when blank");
  await page.screenshot(OUT + PFX + "choose-name-empty.png");

  // 3. Later closes it; the user name stays unset; the sidebar shows the default
  await page.click("#btn-choose-name-later");
  check(await page.eval("!document.querySelector('#choose-name-in')"), "Later closes the modal");
  check((await stored()).name !== "Zed" && !(await stored()).name, "Later does not save a name");

  // 4. reload: back, with the SAME default (persisted)
  await page.goto(site.url);
  await page.waitFor("document.querySelector('#choose-name-in')", { what: "modal again after reload" });
  check((await modalState()).value === auto, "same default after reload", auto);
  // Escape = Later
  await page.key("Escape");
  check(await page.eval("!document.querySelector('#choose-name-in')"), "Escape closes it");

  // 5. reload, Save "Zed" with Enter
  await page.goto(site.url);
  await page.waitFor("document.querySelector('#choose-name-in')", { what: "modal 3rd time" });
  await page.type("#choose-name-in", "  Zed ");
  await page.key("Enter");
  check(await page.eval("!document.querySelector('#choose-name-in')"), "Enter saves and closes");
  check((await stored()).name === "Zed", "trimmed name stored", JSON.stringify(await stored()));
  await page.screenshot(OUT + PFX + "after-save.png");

  // 6. reload: no modal any more
  await page.goto(site.url);
  await L.sleep(1200);
  check(await page.eval("!document.querySelector('#choose-name-in')"), "no modal after a saved name");

  // 7. create a named room: the minted link and the copy-invite link carry the name, the stored link does not
  const name = "Movie Night (Fri!) Ö";
  const minted = await L.createRoom(page, { name, server: RELAY, hash: HASH });
  await L.waitJoined(page);
  const enc = "Movie%20Night%20%28Fri%21%29%20%C3%96";
  check(minted.endsWith(`|${HASH}|${enc}`), "minted link ends with |hash|encoded name", minted);
  const invite = await page.eval("__sss.S.rooms[0].inviteLink()");
  check(invite === minted, "copy-invite link equals the minted link", invite);
  const storedRooms = await page.eval(`JSON.parse(localStorage.getItem("sss.rooms.v1"))`);
  check(storedRooms[0].link === `${minted.split("|").slice(0, 4).join("|")}` && storedRooms[0].name === name, "stored room keeps the plain 4-field link + name", storedRooms[0].link);
  check((await page.eval("__sss.S.rooms[0].members.map((m) => m.name)")).includes("Zed"), "joined under the saved name Zed");
  await page.screenshot(OUT + PFX + "room-created.png");

  // 8. paste a native-formatted 5-field link (encoder output checked against the native vector in tests.js) into Join:
  //    the name field is prefilled; a typed name is never overwritten; an edited name is what is saved
  const plain = minted.split("|").slice(0, 3);
  plain[1] = "1234567890abcdef1234567890abcdef"; // a different room on the same relay
  const nativeLink = `${plain.join("|")}|${HASH}|Gr%C3%B6%C3%9Fe%20%C3%9Cn%C3%AF%20%F0%9F%8E%AC`;
  await page.click("#btn-create-join");
  await page.waitFor("document.querySelector('#add-link')", { what: "join modal" });
  await page.type("#add-link", nativeLink);
  const prefilled = await page.eval("document.querySelector('#add-name').value");
  check(prefilled === "Größe Ünï \u{1F3AC}", "pasted 5-field link prefills the name", prefilled);
  await page.screenshot(OUT + PFX + "join-prefilled.png");
  // typed name wins over a later paste
  await page.type("#add-name", "Mine");
  await page.type("#add-link", `${nativeLink}`.replace("%F0%9F%8E%AC", "X"));
  check((await page.eval("document.querySelector('#add-name').value")) === "Mine", "a name the user typed is never overwritten");
  // clearing the name lets the next paste prefill again
  await page.type("#add-name", "");
  await page.type("#add-link", nativeLink);
  check((await page.eval("document.querySelector('#add-name').value")).startsWith("Größe"), "after clearing the name a paste prefills again");
  await page.type("#add-name", "Edited name");
  await page.click("#btn-join");
  await page.waitFor("__sss.S.rooms.length === 2", { what: "second room added" });
  const rooms2 = await page.eval(`JSON.parse(localStorage.getItem("sss.rooms.v1"))`);
  check(rooms2[1].name === "Edited name" && rooms2[1].link.split("|").length === 4, "stored room has the edited name and no name in its link", JSON.stringify(rooms2[1]));
  await L.waitJoined(page, 1);

  // 9. a 5-field link with an unusable name still joins (name dropped)
  await page.click("#btn-create-join");
  await page.waitFor("document.querySelector('#add-link')", { what: "join modal" });
  plain[1] = "abcdefabcdefabcdefabcdefabcdefab";
  await page.type("#add-link", `${plain.join("|")}|${HASH}|100%`);
  check((await page.eval("document.querySelector('#add-name').value")) === "", "malformed % in the name: no prefill, link still accepted");
  await page.click("#btn-join");
  await page.waitFor("__sss.S.rooms.length === 3", { what: "third room added" });
  await L.waitJoined(page, 2);

  console.log("console errors:", JSON.stringify(A.consoleErrors));
  check(A.consoleErrors.length === 0, "no page console errors");
} catch (e) {
  failed++;
  console.log("FAIL exception:", e.message);
  await page.screenshot(OUT + PFX + "fail.png").catch(() => {});
} finally {
  await A.close();
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASS");
process.exit(failed ? 1 : 0);
