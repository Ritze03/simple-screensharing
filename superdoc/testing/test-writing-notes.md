# Test-writing notes

Moved out of the force-loaded `claude-instructions/testing-and-verification.md` on 2026-11-07 (row 167).

### Tests and the global locale

`rust_i18n`'s locale is process-global and `cargo test` runs tests in parallel. A test that calls
`rust_i18n::set_locale("de")` makes unrelated tests fail at random (`left: "Bildschirmfreigabe", right: "Screen share"`,
task #78). Tests that need another locale either pass it explicitly (`t!(key, locale = "de")`, as
`pill_slots_fit_every_label_in_every_locale` does) or run in a child process with their own locale
(`ui::stream_settings::tests::every_locale_fits_two_columns_at_full_modal_width` re-runs itself per locale with
`SSS_TEST_LOCALE`). Never switch the global locale in-process in a test.

### Tests of "nothing reached the decoder"

Do not prove a negative with `sleep(150 ms)` plus "`decoded_total` did not change" (task #112): frames
already inside a GStreamer decoder keep arriving after the call you are testing, so the counter moves a
little later on a loaded machine (`left: 3, right: 2` in
`deltas_of_the_other_bitstream_family_are_held_until_a_keyframe`, 31 of 100 runs failed under a 40-thread
`stress-ng`, 0 of 200 after the fix). Assert on what the code under test did synchronously instead:
`decode::video::Worker::pushed()` (test-only) counts the access units handed to a pipeline. To hunt such a
flake, run the single test binary 100-200 times with `xargs -P 20` under `stress-ng --cpu 40` (start it yourself,
stop it afterwards).

### Scratch config

The rig launches every client with its own `XDG_CONFIG_HOME`/`XDG_DATA_HOME` in the scratch
dir; the user's saved rooms never auto-connect. If you launch a client outside the rig
(for example `--gallery`) set `XDG_CONFIG_HOME=<scratch dir>` yourself; eframe may also create
`~/.local/share/<app-id>/` storage dirs: delete any you created.

