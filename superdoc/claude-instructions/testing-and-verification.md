# Testing and Verification

**Mandatory working rule for agents, force-loaded from the repo-root `CLAUDE.md`.**

## The rule: verify against the real app by default — always, not just when a change "clearly needs it"

Verify every change — UI included — against the real running app: real Electron shell, real
built `src-tauri` sidecar, driven with Playwright's `_electron` driver per the `run` skill.
This is the default, not a fallback for when you judge a change to be risky enough to deserve
it.

This includes, but isn't limited to:

- any real backend Tauri command (new or changed)
- any settings/state persistence
- any Electron-specific window/IPC behavior (pop-out windows, fullscreen, menu bar, close
  events)
- any real capture/encoder pipeline behavior
- any real relay/network behavior
- **pure CSS/layout tweaks** (padding, sizing, alignment) — yes, even these. Real Electron is
  barely more expensive to verify against than mock+browser once the sidecar is already
  built (which it usually is, from other work this session), and it removes any judgment
  call about whether a change is "provably" identical in both — see **Why this is written
  down** below for why that judgment call itself was the recurring failure, not just missed
  backend coverage.

Serving `frontend/` over plain HTTP and driving it with a browser against mock data proves
nothing about any of the above, and don't reach for the Playwright **MCP** browser tools
either — they attach to a standalone Chrome instance, not the real Electron renderer, so
they're just mock verification with extra steps. Do not treat "the mock UI looked right in a
browser" as verification — it isn't, even if it's tempting because it's faster to set up.

**Why this is written down**: over one session, many dispatched sub-agents working on
`simple-screensharing-client` defaulted to browser+mock verification even for changes that
depend on real Electron/Tauri backend behavior. The project owner had to correct each agent
individually, one at a time. This doc was first written to fix that — but its first version
still carved out a "provably CSS-only" mock exception, and that exception itself caused the
*same* correction to happen again, twice, in the same session, for a pure button-padding
change: an agent (correctly, by the letter of that version of this doc) verified a CSS-only
change against mock+browser, and the project owner asked "why not Playwright against the real
UI?" — twice. The lesson: a rule with a judgment-call exception gets the exception
re-litigated per agent, which is the exact failure mode this doc exists to prevent. Removing
the exception removes the re-litigation. It's documented here so it doesn't have to be
re-explained per agent, per session.

## Is there still a legitimate reason to use mock+browser?

Only when the real app genuinely cannot be launched in the current environment — e.g. no
`src-tauri` sidecar can be built at all (missing toolchain/dependency you can't fix), or no
display server is reachable even headlessly. That's an environment failure, not a judgment
call about the change's category. If you hit one, say so explicitly in your report (don't
silently fall back to mock) so the gap is visible instead of assumed-equivalent.

## How to actually launch the real app

1. Build the sidecar Electron spawns:
   ```
   cargo build -p simple-screensharing-client-tauri
   ```
2. Launch Electron — either manually (`electron .` from `simple-screensharing-client/electron/`)
   for a quick look, or scripted via Playwright's Node `_electron` driver for automated,
   repeatable verification. See the project's `run` skill
   (`simple-screensharing-client/.claude/skills/run/SKILL.md`) for the concrete, tested
   recipe — don't re-derive it from scratch; multiple agents already did that independently
   this session and it was wasted, inconsistent effort.

### Hard safety constraint: never touch HDMI

DP-1, DP-2, and DP-3 are free for testing. **HDMI has the project owner's real active
session on it (maximized).** Never let any real test window or capture region touch the
HDMI output. Check `xrandr --listmonitors` first, every time, before launching or
positioning any window, and before starting any capture. On this machine the layout is
DP-1/DP-2/DP-3 contiguous from x=0 through x=5759, then HDMI-A-1 starting at x=5760 — but
re-check rather than trust that number, since monitor layout can change.

## Scripting the real app

A real Playwright `_electron` driver (Node's `require("playwright").{_electron}`, launched
with `electron.launch(...)` against the real `electron/main.js`) is the established, working
way to script the real app end-to-end — multiple agents did this successfully this session.
It is **not** the same thing as the Playwright MCP browser tools, which attach to a
standalone Chrome instance and cannot drive the real Electron renderer at all; using them
against this app silently verifies nothing. See the `run` skill for the concrete recipe
instead of duplicating it here.
