# GPU safety incidents

Measured incidents behind the GPU rule in `claude-instructions/testing-and-verification.md` (moved here 2026-11-07, row 167; the rule stays force-loaded).

**The rig does not isolate the GPU (2026-11-05, row 148).** It shares the host's real amdgpu. A rig client's VA encoder under a
window-resize stress in sway (random sizes down to about 60x36, several per second) caused VCN page faults, `ring vcn_unified_1
timeout`, a failed ring reset and a full `GPU mode1 reset` with `VRAM is lost`, which aborted the user's whole desktop session and its running apps. So **no resize stress or rapid encoder restarts on a VA encoder on this machine** unless the user okays that run; use
software encoding (x264, `CaptureDomain::System`) for that logic. At the first `mmhub page fault` or `vcn_unified` line in
`journalctl -k`, stop the run and report; do not continue.

**Measured again 2026-11-07 (row 148, user: "Push and test it (crashes shouldnt happen anymore)"): a *gentle* VA run still faults.** Window share
in the rig, `vah264enc`, window sizes >= 640x360, one change per 3-6 s: the 4th change (858x977) produced `[mmhub] page fault ... address
0x0 ... Faulty UTCL2 client ID: VCN0` in the sharer's encoder thread, then `ring vcn_unified_0 timeout` and `Ring vcn_unified_0 reset
succeeded` (no full reset). A kernel-log watcher (`.tl-scratch/kwatch`-style: poll `journalctl -k` every 2 s, `hs-rig.sh down` on a hit) ended
the run at once. So **no VA resize test at all** (not even gentle) until the capture-restart teardown is fixed; software encoding
(`LIBVA_DRIVERS_PATH=/nonexistent SSS_CAPTURE_PATH=system` in `CLIENT_ENV_1`/`_2`, the log must say `using software encoder`) took 120 s window
stresses with sizes down to 40x30 and bursts every 60 ms without a kernel message. Details: `features/stream/capture-sources.md` "Resize recovery verified".
