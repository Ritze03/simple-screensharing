# Website on a phone

Phone dropouts, decoder failures, which phones can watch, and the Screen Wake Lock. Part of the browser client, see `superdoc/features/website.md`.

## Phone dropouts: what can end a watch, the connection log, and the relay's control-frame bug (row 146, 2026-11-05)

*Why:* the user on an iPhone 15 Pro Max (Chrome = WebKit) watching 1080p 60 (AV1 and H.264) over Wi-Fi: "frequent dropouts, there is the disconnect sound, then it says 'Waiting for first frame' for a quick time, then it connects again", every few seconds; a friend on Windows + Chrome has it far less often. There is no console on a phone, so the first job was to find which layer drops.

**What a "disconnect sound" and a "Waiting for the first frame" tile can only be** (read from `ui/sounds.js` `RoomSounds`, `ui/rooms.js`, `core/room-client.js`, then driven in a real Chromium viewer, `e2e/dropui.mjs`):

- The "stop" sound (low falling triangle pair) plays only when *another member's stream key vanishes from the room while this page is `joined`*. A viewer's own relay-session loss and reconnect plays nothing here (the snapshot a reconnect delivers arrives in state `reconnecting`, and the room is quiet for 15 s after: `RESUME_QUIET_MS`). The "leave" sound (the same notes, sine, higher) plays for *the sharer* when a viewer of its stream goes away.
- "Waiting for the first frame" (`stage.js:95`) shows only on a *fresh* watch object (`!w.firstFrame`). A relay-session reconnect keeps the watch object (the core re-subscribes), so it never shows that tile; a `watch-ended` plus resume does.
- No viewer-side logic ends a watch: the core ends one only on `StreamStopped` / `MemberLeft` / a stream missing from the post-reconnect snapshot. Loss, a slow decoder, `need-keyframe` (Unsubscribe + Subscribe), the jitter buffer and NACK never remove a stream (`#streamGone` is the only way).

**Measured on this machine** (native sharer in the headless rig, 1080p 60, relay of this branch, headless Chromium 153 viewer; the viewer joined through a UDP proxy that shaped relay -> viewer):

| run | duration | result |
| --- | --- | --- |
| H.264 60, direct | 600 s | 0 session closes, 0 stream losses, 0 sounds, 59.4-59.9 decoded fps |
| H.264 60, 20 Mbit/s, 60 ms queue | 300 s | same, 0 events |
| H.264 60, 10 Mbit/s + 0.3 % loss | 90 s | 0 events |
| H.264 60, 3 % loss + 20 ms | 90 s | 0 events (many `need-keyframe`, all recovered in place) |
| AV1 60, 3 % loss + 20 ms | 120 s | 0 events |
| H.264 60, 30 s total blackhole | 80 s | session closes ("control stream ended"), reconnects after about 10 s, watch survives, **no sound**, no waiting tile |

**Corrected 2026-11-05 (row 150):** these runs decoded in software and say nothing about a phone's hardware decoder, which is where the user's log then pointed; see "Phone decoder failures". What stays true of them: with Chromium a viewer that is merely slow or lossy never drops, and a viewer whose session really closes does not hear a sound and does not see the waiting tile. The symptom therefore needs either a sharer whose session or stream ends (the sound and the new watch are then the *viewers'* experience), or the iPhone's session closing and the sound being the sharer's "leave".

**The bug found, measured with a test that failed before the fix: the relay ends a connection when a control frame arrives in pieces and a datagram arrives between them.** `server/src/relay.rs` `connection_loop` raced `read_control_frame` (two `read_exact` calls) against `connection.read_datagram()` and `accept_uni()` in one `select!`. `read_exact` is not cancel-safe: when the datagram branch wins while a frame is only partly in (its 4-byte length prefix, or a payload split over two packets), the bytes already taken are dropped, the next read starts mid-frame, reads a garbage length or payload, `bail!`s ("control frame length N exceeds cap", "decoding control frame", "control stream closed mid-frame"), and the relay drops that member: `MemberLeft`, its streams stopped. The member's session shows as closed (browser: `control stream ended`), and a native client's too. Test `relay::tests::split_control_frame_survives_a_datagram_in_between` (cut inside the length prefix and inside the payload, a datagram after each cut) failed with `send datagram: ConnectionLost(ApplicationClosed(ApplicationClose { error_code: 0, reason: b"" }))` and passes with `ControlReader` (a persistent buffer filled by the cancel-safe `RecvStream::read`). Datagrams are what makes it likely in the field: a sharer sends media datagrams all the time, a viewer sends NACK requests and Hello/Avatar datagrams exactly while it is losing packets, which is also when it sends Unsubscribe + Subscribe.
*What stays inference:* nothing here proves a real iPhone, or a real Chromium, splits a control frame. Chromium never did in the runs above (every `connection ended` line in the relay log of these runs is an expected close). That WebKit's QUIC stack cuts a small stream write at a packet boundary, or sends it as two STREAM frames, is the open point: a Windows Chrome that splits rarely and an iPhone that splits often would explain "far less frequent", but it is a hypothesis. **A relay deployment is needed for this fix** (website-only changes cannot stop the relay from losing bytes).
Production check that settles it, from the relay's `relay.log` (INFO level, no change needed): `connection ended` lines whose `error=` contains `exceeds cap`, `decoding control frame` or `closed mid-frame`, around the times of a drop. None of those = this is not the cause.

**Evidence the next test produces: Settings > Connection** (`ui/app.js` `connLog`, `store.js` `dropLog`, `rooms.js`). Every line that explains a drop is kept in memory for the page's life (200 lines since row 150, a repeated line is counted `xN`) and shown as selectable text with a Copy button, on a phone too (a fourth tab, "Connection" / "Verbindung"):
- `connection to the relay lost: <reason> (close code N)`: the close code and text the browser reports (the control stream's end now waits up to 150 ms for `wt.closed`, `session.js`, so the code is not lost to a race), `retry N failed: <why>`, `back after X s`;
- `<name>'s video "<label>" is gone: they stopped it | their session on the relay ended | not there any more after our reconnect` (+ `(we were watching it)`): the new core event `stream-gone` (`cause`, also on `watch-ended`);
- `<name>'s <label>: need-keyframe (loss|gap ...|waiting|decoder-error|overload)` and `decoder-error #n (...)`: the viewer-side recoveries (format: "Phone decoder failures").
Reading it: "their session on the relay ended" or "they stopped it" on every viewer = the sharer side (A); `connection to the relay lost` with a close code = the viewer's session (B, and the relay bug above if the relay log has the lines); only `need-keyframe` / `decoder-error` lines and no loss = the decoder (C). `e2e/conn-log.mjs <relay> <hash>` checks all of it against a real relay.

## Phone decoder failures: frames lost whole (row 150, 2026-11-05)

**2026-11-07 (row 151): gaps are now repaired first.** A frame lost whole is re-requested by its `frame_id` (a NACK Request for an unseen frame, `protocol-spec.md` §6.6 "A frame never seen"), later frames are held, and only if no answer comes does the keyframe wait below start. See `features/video/selective-retransmission.md` "A frame lost whole". The Connection log gained fields for it (list after the example below).

*Why:* row 146 ruled the browser decoder out, and that was wrong: its runs used random loss in software decoders on a desktop, which proves nothing about a phone's decoder, and they never removed a frame whole. The user's Connection log from an iPhone 15 Pro Max (Chrome, so WebKit), 5 minutes at 1080p 60, shows no relay session line and no stream-gone line, only this, every 2-60 s, mostly not right after a logged loss:

```
14:54:49 TestPort: Ritze's Screen: decoder-error (Decoder failure)
14:54:49 TestPort: Ritze's Screen: need-keyframe (decoder-error)
14:54:50 TestPort: Ritze's Screen: need-keyframe (loss)
14:54:51 TestPort: Ritze's Screen: decoder-error (Decoder failure)
```

The user reports it on Android too ("Both on IOS and android"), on AV1 and H.264.

**What the error is** (read from the browser sources, 2026-11-05). WebKit: `Decoder failure` is one string, in `RemoteVideoDecoderCallbacks::notifyDecodingResult`, reached when the GPU process sends `FailedDecoding`: its decoder rejected a frame, or gave back no pixel buffer for one that was not reordered. `WebCodecsVideoDecoder` then closes the decoder with an `EncodingError`. So **one undecodable frame is fatal**, and the VideoToolbox status (such as -12909, bad data) is not passed on. Chrome on an iPhone is WebKit. Chromium (Android, desktop): a failed decode is `EncodingError: Decoding error.` (`decoder_template.cc`), `Decoder failure` does not exist there; a codec taken back is a different path, and too many decoders is a `QuotaExceededError` at configure. So the name and message in the log say which browser and which case.

**The hole, proven from the code:** a frame with at least one fragment left is seen by the reassembler, repaired by NACK or reported as loss. A frame with *no* fragment left leaves no trace, NACK cannot ask for what nobody knows exists, and the next delta goes to the decoder on a missing reference. `frame_id` cannot show it: it is the connection's nonce counter, shared with audio, other streams and previews (spec §5.1). Small deltas make this common: a quiet screen's delta is one or a few datagrams, so one lost datagram can be a whole frame. It also explains the log's shape: losses NACK repairs are never logged, and those it cannot see surface only as a decoder error.

**Measured on real decoders** (headless Chromium, real WebCodecs through the real `VideoReceiver`, software decoders; 240 frames of a real encoder's stream, 12 deltas removed whole; `decoder-error`s with the check off / on):

| stream | check off | check on | pictures shown off / on |
| --- | --- | --- | --- |
| libaom realtime, keyframe every 15 | **10** `EncodingError: Decoding error.` | 0 | 134 / 144 |
| libaom with hidden frames, every 60 | **4** | 0 | 32 / 28 |
| rav1e, every 60 | **4** | 0 | 65 / 28 |
| SVT-AV1, every 60 | 0 | 0 | 228 / 28 |
| x264 zerolatency, every 15 | 0 | 0 | 228 / 144 |

Nothing removed: 240 of 240 shown, 0 gaps, every stream. And live, behind a real relay, Chromium's own AV1 encoder and decoder: 10 decoder errors from 12 removed frames, mid-GOP (`sinceKey` 5-8), the shape of the user's log. So a frame lost whole **does** kill a real decoder, with no loss logged. Not every decoder: Chromium's H.264 decoder and SVT's streams carried on, which is why a desktop can look healthy.

**The cost, on a decoder that would have coped:** it now freezes on the last picture until the next keyframe instead of showing a concealed, slightly wrong one (the last column; with 250 ms keyframes a freeze is at most 250 ms, with a long interval the viewer asks for a keyframe). An ordinary logged loss has always done this. *Why accept it:* the viewer cannot tell which decoder it has until it has failed.

**What stays inference:** no phone was available. That a phone's decoder fails the same way, in particular on H.264, and that whole-frame loss is what the phone actually hit, fits the log but is not proven; the next log decides it (below).

**Fix.**
- `codec.js` `RefChain` follows the stream's own picture numbering: H.264 `frame_num` (exact, by the standard), AV1 the shown frame's `order_hint` (`vaav1enc` sets it to the display index since the keyframe). A jump is a **gap**: `VideoReceiver` stops as for any loss, the last picture stays, the next keyframe resumes. Hardware never sees the broken delta. Checked against real software encoders, 240 frames each, 0 false jumps: x264 with and without B-frames, libaom with hidden frames and `show_existing_frame`, SVT-AV1, rav1e. *Why parse the bitstream and not count:* there is no per-stream sequence number on the wire, and a wire change would need native and relay.
- **Self-disarming.** A jump between two consecutive `frame_id`s cannot be loss, so the encoder numbers differently from what is assumed and the check switches off for that watch (`chain=off`). *Why:* the check could not be run against `vah264enc` / `vaav1enc` here (no GPU encoder in tests), and a wrong assumption must cost nothing but the check.
- **Not covered:** H.265, and AV1 without order hints (Chromium's own AV1 encoder writes none, measured: `hintBits: 0`, so a browser sharer's AV1 is not followed). `chain=none` in the log.
- **No resubscribe when a keyframe is due anyway** (`room-client.js` `KEYFRAME_SOON_MS`, 500 ms): `need-keyframe` carries `gopMs`, the longest of the last four keyframe intervals, and the client only asks if it is longer; `waiting` (1 s without a keyframe) always asks. *Why:* with 250 ms keyframes Unsubscribe + Subscribe gained nothing and cost an extra IDR, a moment off the relay's route, and a sound on the sharer (below).
- **Software fallback:** 3 failures in 30 s make the next decoder `prefer-software`, if `isConfigSupported` agrees (`decoder-mode (prefer-software)` in the log). Android only in effect: WebKit's `RemoteVideoCodecFactory` uses the hint only for VP9 on macOS. It is the last resort, not the fix; 1080p 60 in software may be slow on a phone.

**What the user hears and sees.** On the phone nothing plays: "stop" needs a stream key to vanish, and Subscribe announces nothing. On the sharer each Unsubscribe + Subscribe changes the viewer list twice (`notify_viewers`), so the sharer's "leave" and "join" sounds play, native as well as browser: that is the sound in this flow (read from the code, not observed on the user's machine). The picture freezes on its last frame until the next keyframe; a failure never shows the waiting tile, because `w.firstFrame` is per watch object and neither `decoder-error` nor a resubscribe makes a new one (`e2e/phone.mjs` "decoder failure" polls for it every 20 ms). The tile the user saw needed a new watch, i.e. row 146's relay bug.

**The Connection log now carries the next test** (`media-event.js` `mediaLogLine`, 200 lines):

```
decoder-error #3 (EncodingError: Decoder failure) avc1.640c1f 1920x1080 hw=no-preference q=1 key=120ms/8f break=230f out=412 gaps=2 chain=on ptsback=0
need-keyframe (gap 4->6, id +4)
```

`#3` failures of this watch; then the error's name and message, codec string, size, hardware hint; `q` decode queue at the last submit (an errored decoder reports 0); `key` time and frames since the last keyframe; `break` frames since the last loss, gap or decoder failure (`none` = no break yet); `out` pictures this decoder produced (0 = it failed straight after configure); `gaps` seen so far; `chain` on / off / none; `ptsback` frames whose pts did not rise. `id +N` in a gap line is the `frame_id` step.

**Connection log additions (2026-11-07, row 151).** `decoder-error` lines end with ` stale=N repaired=N` (stale deltas dropped so far, frames repaired so far). A successful repair is its own line: `repaired (4->6, id +4, 4ms, 1 rounds, 8 B, 1/1, dropin=3)` = gap from picture 4 to 6, `frame_id` step 4, ms from detection to the frame being back, request rounds used, size of the repaired frame, repaired/gaps totals, `dropin` = datagrams this browser threw away itself (`?` where it does not report that). A gap that could not be repaired is `need-keyframe (gap 4->6, id +4, unrepaired 310ms, dropin=0)` (the `unrepaired` part only after a failed repair). `repaired` lines differ in their numbers, so `dropLog` does not collapse them: at one repair every 2 s the 200-line log holds about 7 minutes.

Reading it: `gap` lines and few or no `decoder-error` = the cause is confirmed and fixed. `decoder-error` at the old rate with `chain=on`, `gaps=0` and a large `break` = whole-frame loss was not the cause; look next at `key` (small: the keyframe itself is refused, a bitstream problem) and `out=0`. `chain=off` or `none` on AV1 = the sender's numbering needs a look.

**Regression tests:** `tests.js` `strictDecoder` is a mock that fails on every delta not following the last frame, using `timestamp` as an oracle independent of `RefChain` (`broken reference: 19000 -> 21000` without the check). It proves what reaches `decode()`, not how a phone behaves. Scenario `frame lost whole` removes every fragment of real frames behind a real relay.

## The website on a phone (row 145, 2026-11-05)

*Why:* the user asked "Can you also make sure that the website works on smartphones as well?". Before this the page had a fixed `#app { min-width: 800px }`, no width `@media`, hover-only tile buttons, right click as the only way to a room's menu, and the stage could not be reached by touch.

**One breakpoint**, `(max-width: 760px), (max-height: 500px) and (pointer: coarse)` = narrow (portrait) or short + touch (landscape). It is written twice, in `css/app.css` (the last block, "phones") and in `store.js` `PHONE_Q` / `PHONE` (`matchMedia`, for what CSS cannot decide); change both. Desktop rules are untouched (`stage.mjs` 227/227 and `tests.html` 124/124 as before). `ponytail:` no tablet tier: a tablet gets the desktop layout when wider than 760 px.

| area | on a phone |
| --- | --- |
| rooms list | a drawer over the stage (`.p-side` fixed, `S.drawer`); the menu button in every header band opens it (`#btn-drawer`), the bar's sidebar button does in landscape (`#btn-sidebar`); closed by picking a room or Watched, by a tap on the scrim or by opening any dialog |
| landscape | the header band is hidden where a stage has its own bar (`main:has(.stg)`), strip avatars 32 px, bar 56 px |
| room menu | **long-press** on a sidebar room (500 ms, `dom.js` `longPress`: iOS Safari never sends `contextmenu`; Android does and opens the same menu); the press swallows its own click; `-webkit-touch-callout: none` on the rows |
| tiles | the name tag and the three buttons (pop-out, fullscreen, `⋯` stream menu) are always shown (`hover: none`), 40 px; the pop-out button is dropped when it would only be the fullscreen button again (no Document PiP) |
| people strip | a tap opens the hover card, a tap elsewhere closes it (`stage.js` touch `pointerdown`); the viewers chip too |
| fullscreen | a tap shows the auto-hidden chrome (the document `pointerdown` already fed `chromeSync`); where `requestFullscreen` is missing (iPhone Safari) the stage-only layout stays as a pseudo fullscreen and the bar's button leaves it; the tile's fullscreen button is always there (row 152: without element fullscreen it opens the fixed full-viewport layer) |
| layouts | Split and Crop only: **Floating is hidden** (not offered, a saved `float` shows as Split: `layoutOf`), and so are the resize bars and window handles (a 3 px bar under a finger is a mis-drag; nothing scrolls in the stage). Reset layout stays |
| bar | wraps by the existing container query; buttons 44 x 40, arrows 30 wide |
| menus, dialogs | menus 44 px rows and scroll within the viewport; dialogs `calc(100% - 16px)` wide, their body scrolls, foot wraps, `100dvh`; fields 16 px (iOS would zoom), a focused field scrolls into view (`focusin` + `visualViewport` resize; `interactive-widget=resizes-content` in the viewport meta, `viewport-fit=cover` + `env(safe-area-inset-*)`) |
| sharing | where `getDisplayMedia` is missing, Share screen and Share audio are replaced by one dimmed button that says why on tap (`ui.err.no_screen_share`); Webcam stays when `getUserMedia` works; the Document PiP buttons are hidden where unsupported (already so) |
| unsupported browser | the banner now names what is missing (`WebTransport`, `WebCodecs`, `https`) next to the old text |

**Stream size after a sender's resize** (row 141): checked, nothing to fix. The tile aspect uses the decoded canvas (`itemAspect`), and the resolution label / stats use `watchStats().width` = the decoded frame's `displayWidth` (`video-rx.js`), the announced `StreamInfo` size only for a stream that is not being watched.

### Which phones can watch (checked 2026-11-05)

Needs WebTransport with `serverCertificateHashes` (the relay's self-signed certificate is pinned) and WebCodecs `VideoDecoder`, over https.

| browser | watch | share screen | share webcam |
| --- | --- | --- | --- |
| Chrome / Edge / Samsung Internet on Android | yes (Chrome for Android lists `serverCertificateHashes` from 157; older ones: no pinning, so only a relay with a CA certificate) | no (`getDisplayMedia` does not exist on Android) | yes (`getUserMedia` + WebCodecs encode + `MediaStreamTrackProcessor`) |
| Firefox for Android | no (WebCodecs missing in 159 per caniuse) | no | no |
| Safari on iOS 26.4 or newer (and every iOS browser, they are WebKit) | yes (WebTransport + `serverCertificateHashes` from 26.4, WebCodecs from 26.0) | no (no `getDisplayMedia` on iPhone) | not tried: needs `MediaStreamTrackProcessor`, which WebKit lacks, so the button stays disabled |
| iOS before 26.4 | no (no certificate pinning; WebCodecs partial) | no | no |

Sources: caniuse "WebTransport serverCertificateHashes" (Chrome Android 157, Firefox Android 159, Safari / iOS Safari 26.4), caniuse "WebCodecs" (iOS Safari full from 26.0, partial 16.4-18.7, Firefox Android none), MDN `WebTransport` (Baseline 2026, newly available since March 2026). The iOS rows rest on caniuse and MDN data, not on a device; the WebKit release notes were not found. Only a real phone can prove them.

### Tests

`docs/dev/e2e/phone.mjs <relay> <hash>`: headless Chromium with phone emulation (`mobile: true`, touch emulation, coarse pointer, no `getDisplayMedia`, no Document PiP) and real `Input.dispatchTouchEvent` only; Alice shares a synthetic screen, the phone joins through the dialog, watches, and the run asserts a really decoded stream (960x540), `scrollWidth <= innerWidth` and nothing sticking out on every screen, 40 px tap targets, every menu and dialog inside the viewport, the long-press room menu, the drawer, tap-open people card, fullscreen chrome by tap, the no-screen button, the unsupported-browser message; at 390x844, 844x390 and 360x640. Screenshots go to `E2E_OUT`. What it cannot show: a real on-screen keyboard, iOS-only behaviour (no `contextmenu`, no element fullscreen, safe-area insets), real GPU decode and thermal behaviour.

### Screen Wake Lock (2026-11-05)

*Why:* the user: "On the website when I open it with the phone my screen keeps getting darker and turns off eventually. Make sure to fix that. Should be simple." The viewer draws into a `<canvas>` (`CanvasRenderer`), so the phone sees no playing `<video>` and lets the screen sleep.

`docs/js/ui/wakelock.js` `syncWakeLock()`, called by every `render()`, holds `navigator.wakeLock.request("screen")` while any room has a watch, a screen or webcam share has a destination (`allDests()`), or, on a phone (`PHONE`), any room is joined (watching or not). It releases when none holds. The browser drops the lock when the page is hidden, so `visibilitychange` to visible re-requests; a rejected request (Safari refuses without a user gesture) is retried on the next render or `pointerdown`. A `pending` flag stops duplicate requests, and a lock granted after it stopped being wanted is released at once. No `navigator.wakeLock`, or a rejection: silent, no toast. Checked by `e2e/phone.mjs` against the real lock in headless Chromium (requests spied); only a real iPhone proves Safari/WebKit behaviour.
