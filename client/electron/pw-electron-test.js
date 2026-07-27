// Throwaway script: prove Playwright can drive the real Electron app end to end.
const { _electron: electron } = require("playwright");
const path = require("path");
const fs = require("fs");

const ELECTRON_DIR = "/home/mo/GitHubProjects/simple-screensharing/simple-screensharing-client/.claude/worktrees/agent-a4709e6b96ed2347a/electron";
const SCREENSHOT_DIR = __dirname;

const consoleLog = [];

(async () => {
  const electronApp = await electron.launch({
    args: [path.join(ELECTRON_DIR, "main.js")],
    cwd: ELECTRON_DIR,
  });

  electronApp.on("window", async (win) => {
    console.log("[harness] new window:", win.url());
  });

  const page = await electronApp.firstWindow();

  page.on("console", (msg) => {
    const line = `[page-console:${msg.type()}] ${msg.text()}`;
    consoleLog.push(line);
    console.log(line);
  });
  page.on("pageerror", (err) => {
    const line = `[pageerror] ${err.message}`;
    consoleLog.push(line);
    console.log(line);
  });

  console.log("[harness] waiting for nav...");
  await page.waitForSelector("#nav", { timeout: 20000 });
  await page.waitForSelector("nav#nav button", { timeout: 20000 });

  const navButtons = await page.locator("nav#nav button").allTextContents();
  console.log("[harness] nav buttons found:", navButtons);

  const tabs = ["Stream Control", "View", "Settings"];
  const results = {};

  for (const tab of tabs) {
    await page.getByRole("button", { name: tab, exact: true }).click();
    await page.waitForTimeout(300);
    const panel = page.locator(`.tab-panel[data-tab="${tab}"]`);
    const isHidden = await panel.getAttribute("hidden");
    const text = await panel.innerText();
    results[tab] = { hiddenAttr: isHidden, textSnippet: text.slice(0, 200) };
    const shotPath = path.join(SCREENSHOT_DIR, `tab-${tab.replace(/\s+/g, "-").toLowerCase()}.png`);
    await page.screenshot({ path: shotPath });
    console.log(`[harness] tab="${tab}" hidden=${isHidden} screenshot=${shotPath}`);
  }

  // Sanity: no crash / no error dialog. Check title & that window is still open.
  const title = await page.title();
  console.log("[harness] final page title:", title);

  fs.writeFileSync(
    path.join(SCREENSHOT_DIR, "tab-results.json"),
    JSON.stringify({ navButtons, results, consoleLog }, null, 2)
  );

  await electronApp.close();
  console.log("[harness] closed cleanly");
})().catch((err) => {
  console.error("[harness] FAILED:", err);
  process.exit(1);
});
