# Glossary details

Detail that used to sit in `TERMINOLOGY.md` entries and is on no other page. The glossary entry
stays one short line and links here. One `##` per term.

## GUI lock

Historical. Until 2026-10-14 agents held a `flock` (fd 9) on a lock file in the temp directory around
every interactive GUI session on the user's real, shared desktop (launch, focus, synthetic input,
screenshot). The headless rig is private per worktree and needs no lock. Only the legacy scripts and the
`SSS_GUI_LOCKED=1` opt-in name of the browser harness (`docs/dev/README.md`) still mention it. History:
`testing/rig-reference.md`.

## Legacy (real-desktop) script

A script that acts on the user's real desktop session (the compositor's own control tool, synthetic
input through uinput, the real portal picker). It carries a `LEGACY (2026-10-14)` banner. The repo-root
`launch-test*.sh` and `docs/dev/e2e/native-session.sh` also refuse to run without
`SSS_ALLOW_REAL_DESKTOP=1`, which is for the user's own manual use. Agents never run them. List:
`client/scripts/README.md` "Legacy".

## On air vs starting

A share's phase in a room (`ui::MyPhase`). **On air**: at least one destination there is Live, Rebuilding
or Stalled. **Starting**: none is on air yet but one is still Starting (for example waiting for the
portal's first frame). It drives the sidebar cast icon and the "Live in N rooms" line. Screens only: the
audio button's state is separate (`features/audio/desktop-audio.md`). Idle-monitor shares:
`features/stream/capture-sources.md`.
