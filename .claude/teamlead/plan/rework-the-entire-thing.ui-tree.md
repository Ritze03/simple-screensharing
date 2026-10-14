# UI container tree — spec for the egui port (D18)

Source: mockup v3, https://claude.ai/artifact/5yA1iHf1YnXqmABkZajCe4 (Proposed tab), version 1791932813-b6dd.
Primitives: `V` = vbox (`ui.vertical`), `H` = hbox (`ui.horizontal`), `slotN` = fixed slot (`add_sized` / `allocate_exact_size`),
`fill` = remaining width. Gaps = item_spacing steps 0/4/8/12/16. Frames only paint + inner margin, never lay out.
Tokens: `--band-h` 56 px (brand, room header, user area, action bar, modal head), `--ctl-h` 32 px, `--ctl-h-lg` 36 px
(bottom bar, modal footers), `--pill-h` 18 px, row 32 px, sub-row 24 px, label slot 120 px, value slot 72 px.
Card/tile grids: vbox of hbox rows of equal fill columns, cols = clamp(floor((avail+12)/272), 1, 4), short rows padded.
Row patterns are single functions: ctrlRow, roomRow, sharerRow, personRow, destRow, popItem, btnRow, card, gridRows, modalShell.

## Main window

```
window · hbox (g0)
├─ SidePanel::left · vbox (272 px, g0)
│  ├─ hbox·band (brand)                 [mark][label]                  56 px
│  ├─ ScrollArea · vbox (g8)
│  │  ├─ Frame · vbox (well, g0)
│  │  │  └─ hbox·room-row               [slot18 icon][fill name][slot28 num][slot22 unit]   32 px
│  │  ├─ hbox·heading                   [section label][rule]
│  │  ├─ Frame · vbox (well, g0)
│  │  │  ├─ hbox·room-row               [slot18 dot][fill name][slot18 live][slot28 num][slot22 unit]   32 px
│  │  │  └─ hbox·sharer-row (×n)        [slot18 indent][slot18 icon][fill name][slot28 num][slot22 unit "fps"]   24 px
│  │  └─ hbox·row                       [fill: Create or join button]
│  └─ TopBottomPanel::bottom · hbox (user band)   [slot32 avatar][fill vbox·who(name, status)][slot32 gear]   56 px
└─ CentralPanel · vbox (g0)
   ├─ TopBottomPanel::top · hbox (header band)   56 px
   │  ├─ hbox·title (fill)               [h2][status pill][meta]
   │  └─ hbox·room-controls              [Auto-connect][Copy invite link][Leave][⋯]   all 32 px
   ├─ hbox·body (fill, g0)
   │  ├─ ScrollArea · vbox (fill, g16)
   │  │  ├─ hbox·banner (reconnecting)   [slot18 icon][fill text][Retry now]
   │  │  ├─ vbox·section (g12)           [hbox·heading][vbox·tile-grid]
   │  │  │  └─ vbox·tile-grid → hbox·grid-row → fill·col → Tile frame
   │  │  │     └─ overlay · vbox         [hbox·top: fill(hbox·statbar), hbox·actions][fill·spacer][hbox·bottom: hbox·audio-pill [slot25][slot110][slot34]]
   │  │  └─ vbox·section (g12)           [hbox·heading][vbox·card-grid]
   │  │     └─ vbox·card-grid → hbox·grid-row → fill·col → Frame · vbox (share card)
   │  │        ├─ thumb frame → overlay · vbox → hbox·corners [LIVE chip][fill][audio chip]
   │  │        └─ vbox·info (pad 12, g8) [hbox·who: slot22 avatar, name, fill title][hbox·specs][hbox·buttons: fill btn, fill btn]
   │  └─ SidePanel::right · vbox (232 px, g12)   [hbox·heading][vbox·people → hbox·person-row [slot22 avatar][fill name][slot44 pill][slot18 eye] 32 px]
   └─ TopBottomPanel::bottom · hbox (action band)   56 px
      ├─ Live:    [slot84 pill][fill vbox·status-text][hbox·audio-control: slot36 mute, source btn][Stream settings][N rooms ▾][sep][Stop sharing]
      ├─ Staged:  [slot84 pill][fill vbox·status-text][audio][Stream settings][sep][Discard][Go live]
      ├─ Idle:    [Share screen ▾][Share audio][fill status text]
      └─ Paused:  [slot84 pill][fill vbox·status-text]
```

## Stream settings modal

```
Modal · vbox (stream settings) (g0, width ≤1120)
├─ hbox·head band (56 px)          [fill vbox·title(h3, sub)][slot96 "Applying" pill][slot32 close]
├─ ScrollArea · vbox
│  └─ hbox·columns (theme::columns 2, g12, top-aligned)
│     ├─ vbox·column (where it goes) (fill, g12)
│     │  ├─ Frame · vbox (card) "Source"   [hbox·row Screen: slot120 | fill readout | slot72 Change(staged)]
│     │  │                                 [hbox·row Audio: slot120 | fill select | slot72 meter][hbox·row: slot120 empty | fill Mute checkbox]
│     │  ├─ Frame · vbox (card) "Sending to"/"Goes live in"
│     │  │  ├─ vbox·dest-list (g4) → hbox·dest-row [slot18 icon][fill name][slot84 pill][slot76 kbps][slot32 ×]   40 px
│     │  │  └─ hbox·buttons [fill Also send to…][fill Move to…]   (live only)
│     │  └─ Frame · vbox (card) "Upload bandwidth"
│     │     [hbox·row Mode: slot120 | fill segmented]
│     │     [hbox·row Total maximum: slot120 | fill slider | slot72 value]
│     │     [hbox·row Floor per stream: slot120 | fill slider | slot72 value]
│     │     [hbox·row Per stream: slot120 | fill readout(right-aligned) | slot72 value]  or  [hbox·banner over budget]
│     └─ vbox·column (how it's encoded) (fill, g12)
│        ├─ Frame · vbox (card) "Video"     [Framerate: slot120|fill select][Bitrate: slot120|fill slider or readout|slot72][Codec: slot120|fill segmented]
│        └─ Frame · vbox (card) "Encoder"   [Preset: slot120|fill segmented(4)][hbox·collapsing-header]
│           └─ vbox·collapsing-body        [Tune][Quality level][Rate control][Keyframe interval: slot120|fill slider|slot72]
└─ hbox·footer (64 px)
   ├─ Live:   [Stop sharing in X][fill·spacer][hbox·note "Saved"][Done]
   └─ Staged: [Discard][fill·spacer][Later][Go live in X]
```

Other modals follow `modalShell`: hbox·head band / ScrollArea vbox of cards with ctrlRows / hbox·footer `[fill·spacer][buttons]`.
Popups: `Popup · vbox` of `hbox·heading` and `hbox·item [slot18 icon][fill vbox(label, sub)][slot23 meter?]`.
