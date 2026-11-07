# Audio sharing

Audio capture, mixing, delay and playback: how a share picks its sound, how it is clocked and kept in sync with the picture, how the viewer levels it, and how it behaves when the source app pauses, vanishes or is switched. This page is a hub; the detail lives in dated task blocks on the pages below (a block keeps its date and its `Superseded` / `Corrected` banner). Where audio first grew up: `features/stream/share-audio-attachment.md` ("Sharing audio ...", "The 2026-07-28 audio bugs"), `features/video-pipeline.md` ("Audio follows video") and the module docs of `client/src/audio.rs` and `client/src/decode.rs`.

- [`audio/desktop-audio.md`](audio/desktop-audio.md): Desktop audio, audio-only shares, the exclusion list, source groups, the audio settings menu (I7) and the notification sounds (I9).
- [`audio/sync-and-clocks.md`](audio/sync-and-clocks.md): the viewer-side audio delay (#91, with its known limits) and the pause/resume saga: two clocks (#128), sender dither (#129), #125, #123.
- [`audio/levelling-and-volume.md`](audio/levelling-and-volume.md): viewer loudness levelling (#119), sender boost and calibration (#114), volume up to 500 % (#96), wheel and live drag (#113, #135), the share meter (#115).
- [`audio/pipewire-lifecycle.md`](audio/pipewire-lifecycle.md): the private null sink (#121), "the shared audio is a name" (#105), a vanished and returning stream (#117), switching source (#118).

Superseded notes: the pre-I7 blocks (#105, #114, #115, #119) carry banners saying what moved; trust the I7 page and the code (`audio.rs` `AudioChoice`, `wanted_ids`, `spawn_sink`, `sync_links`) over them.
