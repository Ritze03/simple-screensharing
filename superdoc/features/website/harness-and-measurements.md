# Website: harness and measurements

Measured numbers and how to run the browser test harness. Part of the browser client, see `superdoc/features/website.md`.

## Measured (this machine, Chromium 153, local relay; full tables in `docs/dev/README.md`)

- Native sender to browser viewer: **H.264 and AV1 at 60 and 120 fps** received at full rate
  (e.g. `vah264enc` 1080p120 gave 120.1 fps, 0 loss). **H.265 only with headed VAAPI decode**
  (60 fps); 0 frames headless.
- Browser sender is limited by the browser's *software* encoder (H.264 ~150-190 fps, AV1 ~90-110 fps
  at 1080p), not by transport or decode; 4K60 is not sustainable.
- Browser to native: H.264 and AV1 at 60 fps decode on the native hardware path.
- Measure with `--flag --disable-frame-rate-limit --flag --disable-gpu-vsync`, otherwise headless
  Chromium's 60 Hz compositor caps `canvas.captureStream` and every 120/240 number is meaningless.

## Working on it

- **Run locally under the real sub-path:** `node docs/dev/headless.mjs serve --port 8123`, open
  `http://127.0.0.1:8123/simple-screensharing/`. `headless.mjs` serves `docs/` under
  `/simple-screensharing/` on purpose, to prove nothing depends on being at the root.
- **Unit tests:** `node docs/dev/headless.mjs run dev/tests.html`. **Scenarios** against a throwaway
  relay (`docs/dev/relay.sh`, tmux, prints the cert hash): `docs/dev/scenarios.mjs` (the behaviour scenarios live in `docs/dev/scenarios.js`; list and count in `docs/dev/README.md`). **Matrix/native interop:**
  `docs/dev/matrix.mjs`, `docs/dev/native.mjs`. **Real UI end to end:** `docs/dev/cdp.mjs` (a
  zero-dependency DevTools driver) + `e2e-lib.mjs` + `e2e/`.
- **Firefox:** `BROWSER=firefox` runs the same harness in the system Firefox (the user's browser is Firefox-based, Gecko); `docs/dev/xbench.mjs` benches decode/render per browser. See `docs/dev/README.md` "Firefox (Gecko)".
- **Do not** pass `--use-fake-ui-for-media-stream` for `getDisplayMedia` tests (it picks a
  `screen:0:0` source that yields no frames); use `canvas.captureStream()` or the harness runner.
- Headed runs only inside the headless sway rig (`hs-rig.sh run -- env SSS_GUI_LOCKED=1 HEADED=1 ...`, see `docs/dev/README.md`), never on the user's desktop (user request 2026-10-14); `SSS_GUI_LOCKED=1` is the
  harness's historical opt-in guard.
- `window.__sss` exposes the UI state for debugging. `index.html#join=<link>` opens the join dialog
  prefilled.
