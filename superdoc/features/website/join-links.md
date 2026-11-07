# Website: join links

Display name and room name in join links. Part of the browser client, see `superdoc/features/website.md`.

## Display name and room name in join links (task #69, 2026-10-15)

Three user requests, the browser mirrors native tasks #68 and #70 so both clients behave the same:

- **Default display name `user-xxxx`.** It used to be "friend" (`ui.default_name`, gone). Now `user-` + 4 random `[a-z0-9]`
  (`crypto.getRandomValues`, rejection-sampled), generated once and persisted under its own localStorage key
  (`sss.autoname.v1`, `store.js` `autoName()`), so it is stable across reloads. `settings.name` stays `""` while the name is
  automatic, which is how the UI tells "auto default" from "chosen" (native keeps `display_name == ""` as unset the same way);
  `displayName()` = chosen name or the auto one, used for the connect and for "you". *Why separate from the settings name:*
  the "Choose your name" modal has to know whether the user ever chose, and an auto name stored as the name would hide that.
- **"Choose your name" modal** (`app.js` `promptForName`, strings `ui.modal.choose_name_title` / `_body`, en + de from
  `client/locales`): on page load, while `settings.nameSet` is false, one small modal (the default 520 px width) opens as soon as no
  other modal is open (a `#join=` dialog at load goes first, like native "first time nothing is open"). The field is prefilled with the
  current default, focused and selected. **Save** (primary, Enter) trims and stores it (saving the untouched default counts, it
  becomes the chosen name), disabled while blank; **Later** (also Escape, x, backdrop) only closes it until the next load. `nameSet`
  turns true on Save and on any non-blank name typed in Settings, and is never cleared again (emptying the settings field later does
  not bring the modal back); profiles that already have a name never see it. *Why:* user request 2026-10-15, make people pick a
  name instead of silently being "friend"/an OS username. *Caveat (same as native, not fixed):* the name is sent at connect, so
  rooms that auto-connect before the user saves keep the old name in their peers' rosters until the next reconnect.
- **Room name in the join link** (5th field, `protocol-spec.md` §11.4, `docs/js/core/joinlink.js`): every copy-invite path
  (`RoomEntry.inviteLink()`: room header button, room menu, the Create dialog's minted link) appends the room's saved name (not the
  "Room abcd1234" placeholder); the join dialog prefills its name field from a pasted link with the native rule (`prefillName`: only when
  the field is empty or still holds the previous prefill, so a typed name is never overwritten and clearing the field re-enables it);
  saved rooms keep the plain 3/4-field link in localStorage (`stripLinkName`; the name is a local, editable label, not identity).
  *Why:* user request 2026-10-15 (copy link carries the name, pasting fills it). The encoder escapes `!'()*` on top of
  `encodeURIComponent` so a link is byte-identical whichever client made it.

- **Server address needs a port** (task #84, user report 2026-10-15; `protocol-spec.md` §11.4): `parseServerAddr`
  (`docs/js/core/joinlink.js`, twin of native `parse_server_addr`) rejects a bare host (a browser would quietly use 443 for
  `https://host/`). The create/join dialog shows an inline hint (`#add-hint`, updated on input without a re-render so focus stays)
  and disables Create/Join; `createRoom` throws, `RelaySession.open` throws `ConnectError` code `server-no-port` / `server-bad-port`
  (so a saved portless room shows `ui.err.server_no_port`). A DNS failure cannot be told apart in the browser (WebTransport reports
  a generic handshake failure). Checked with `docs/dev/e2e/server-port.mjs` (no relay needed).

Verified through the real UI with `docs/dev/e2e/name-modal.mjs` (headless Chromium, throwaway relay) and by the unit vectors
in `docs/dev/tests.js`.
