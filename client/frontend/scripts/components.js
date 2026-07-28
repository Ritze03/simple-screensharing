// components.js — shared UI primitives for the frontend shell.
//
// These are the vanilla-JS equivalent of `theme.rs`'s widget-helper family
// (`primary_button`, `secondary_button`, `danger_button`, `icon_button`,
// `nav_button`, `status_dot`, `card`, ...): small factory functions that
// return a DOM element, styled from `tokens.css` custom properties only
// (never a hard-coded color/size inline), so later workers building the
// Stream Control / View / Settings tab content can reuse them instead of
// hand-rolling their own button/card/dot markup.
//
// Every function here is pure with respect to the DOM: it builds and
// returns an element, wiring only the event listeners it's explicitly given
// (an `onClick` callback, etc.) — none of them mutate global state or reach
// into `shell.js`'s mock data directly. That keeps them reusable from any
// tab's own script without a circular dependency on shell.js.
//
// Exposed on `window.Components` (no bundler / module system in this repo,
// per the frontend's "plain static HTML/CSS/JS" scope) so any script that
// loads after this one can call e.g. `Components.renderRoomRow(...)`.

(function () {
  "use strict";

  /** Small helper: create an element, assign props/attrs, append children. */
  function el(tag, opts, children) {
    const node = document.createElement(tag);
    opts = opts || {};
    if (opts.className) node.className = opts.className;
    if (opts.text !== undefined) node.textContent = opts.text;
    if (opts.html !== undefined) node.innerHTML = opts.html;
    if (opts.attrs) {
      for (const [k, v] of Object.entries(opts.attrs)) node.setAttribute(k, v);
    }
    if (opts.onClick) node.addEventListener("click", opts.onClick);
    (children || []).forEach((c) => c && node.appendChild(c));
    return node;
  }

  /**
   * Wire a `wheel`-scroll listener onto a `<input type="range">` so hovering
   * it and scrolling steps the value up/down by exactly the slider's own
   * `step` (matching the granularity a manual drag/arrow-key already uses),
   * instead of doing nothing (native range inputs ignore wheel scroll in
   * Chromium/Electron) or letting the page/container scroll underneath it.
   *
   * `Why: scroll-up = increase` — matches the typical OS volume/spinner
   * convention (macOS/Windows volume sliders, most native spinbuttons):
   * scrolling "up" (negative `deltaY`) raises the value, "down" lowers it.
   *
   * Dispatches both `input` (for any live-preview readout wired on
   * `input`) and `change` (for whatever handler actually applies/persists
   * the value) so a wheel tick behaves like a real drag-then-release, not
   * just one half of it.
   */
  function attachWheelStep(sliderEl) {
    sliderEl.addEventListener(
      "wheel",
      (event) => {
        event.preventDefault();
        const stepAttr = sliderEl.step;
        const step = stepAttr && stepAttr !== "any" && Number(stepAttr) ? Number(stepAttr) : 1;
        const min = sliderEl.min !== "" ? Number(sliderEl.min) : -Infinity;
        const max = sliderEl.max !== "" ? Number(sliderEl.max) : Infinity;
        const delta = event.deltaY < 0 ? step : -step;
        const next = Math.min(max, Math.max(min, Number(sliderEl.value) + delta));
        sliderEl.value = String(next);
        sliderEl.dispatchEvent(new Event("input", { bubbles: true }));
        sliderEl.dispatchEvent(new Event("change", { bubbles: true }));
      },
      { passive: false }
    );
  }

  /** A single Material Symbols glyph, e.g. icon("expand_more"). */
  function icon(name, extraClass) {
    return el("span", {
      className: "material-symbols-outlined" + (extraClass ? " " + extraClass : ""),
      text: name,
    });
  }

  // -------------------------------------------------------------------
  // Buttons — mirrors theme.rs's four button variants.
  // -------------------------------------------------------------------

  /** Solid accent fill — the one primary action on a screen. */
  function primaryButton(label, onClick, opts) {
    opts = opts || {};
    return el("button", {
      className: "btn btn-primary" + (opts.className ? " " + opts.className : ""),
      text: label,
      onClick,
      attrs: opts.disabled ? { disabled: "disabled" } : {},
    });
  }

  /** Neutral fill + border — the default for everything else. */
  function secondaryButton(label, onClick, opts) {
    opts = opts || {};
    return el("button", {
      className: "btn btn-secondary" + (opts.className ? " " + opts.className : ""),
      text: label,
      onClick,
      attrs: opts.disabled ? { disabled: "disabled" } : {},
    });
  }

  /** Transparent fill, danger-colored text/border — destructive actions. */
  function dangerButton(label, onClick, opts) {
    opts = opts || {};
    return el("button", {
      className: "btn btn-danger" + (opts.className ? " " + opts.className : ""),
      text: label,
      onClick,
      attrs: opts.disabled ? { disabled: "disabled" } : {},
    });
  }

  /** Compact, chrome-free icon-only button for dense list rows. */
  function iconButton(glyph, onClick, opts) {
    opts = opts || {};
    const btn = el("button", {
      className: "btn btn-icon" + (opts.className ? " " + opts.className : ""),
      onClick,
      attrs: opts.title ? { title: opts.title, "aria-label": opts.title } : {},
    });
    btn.appendChild(icon(glyph));
    return btn;
  }

  /**
   * A chunky, full-width nav-rail pill — mirrors `theme::nav_button`.
   * Accent-filled + dark text when active, transparent (hover-only lift)
   * otherwise.
   */
  function renderNavButton(label, active, onClick) {
    return el("button", {
      className: "nav-btn" + (active ? " nav-btn-active" : ""),
      text: label,
      onClick,
      attrs: { "data-tab": label },
    });
  }

  // -------------------------------------------------------------------
  // Status dot — mirrors `theme::status_dot`.
  // -------------------------------------------------------------------

  /**
   * Three states, matching APP-SHELL.md's room-row table:
   *  - "joined"       -> solid filled SUCCESS dot
   *  - "reconnecting" -> pulsing WARNING dot (CSS animation)
   *  - "disconnected" -> hollow TEXT_FAINT ring
   */
  function statusDot(state) {
    return el("span", { className: "status-dot status-dot-" + state });
  }

  // -------------------------------------------------------------------
  // Status vocabulary — four families (progress/live/degraded/failed),
  // matching `capture::DestinationState`'s real variants (see
  // `src/capture.rs`: `Starting`/`Live`/`Rebuilding`/`Stalled`/`Failed`).
  // Visual rules live in `status.css`'s `.status-badge*`/`.status-failed-*`
  // classes; this is just the DOM-building side, one function callers reach
  // for instead of hand-rolling a pill per file.
  //
  // `degraded` is the one family with a live-updating part: it always shows
  // an elapsed-seconds count next to the label (e.g. "WAITING FOR CAPTURE ·
  // 4s"). No DTO carries a "since" timestamp for `Stalled` today (out of
  // this pass's frontend-only scope to add one) — callers that want the
  // counter to be accurate pass `options.since` (a client-side `Date.now()`
  // recorded the first time they observed this state, e.g.
  // `stream-control.js`'s `destStalledSince` map). The badge owns its own
  // `setInterval` (mirrors `tile-video.js`'s fps-interval pattern) and
  // stops itself once its node leaves the DOM — checked on each tick via
  // `isConnected` rather than requiring every caller to remember to call a
  // `.destroy()`, since this codebase's render paths don't have a uniform
  // unmount hook to call one from.
  // -------------------------------------------------------------------

  /**
   * @param {'progress'|'live'|'degraded'|'failed'} family
   * @param {string} label - short, stable, e.g. "STARTING" / "LIVE" /
   *   "WAITING FOR CAPTURE" / "Share failed". Failed's `label` is a full
   *   plain-language headline, not a state keyword.
   * @param {object} [options] - degraded: { since: number (ms epoch) }.
   *   failed: { errorText, onRetry, onDismiss }.
   */
  function statusBadge(family, label, options) {
    options = options || {};

    if (family === "failed") {
      const box = el("div", { className: "status-failed-box" });
      const head = el("div", { className: "status-failed-head" });
      head.appendChild(icon("error", "status-failed-icon"));
      head.appendChild(el("span", { className: "status-failed-headline", text: label }));
      box.appendChild(head);
      box.appendChild(
        el("div", {
          className: "status-failed-sub",
          text: options.errorText || "No further error detail is available for this destination.",
        })
      );
      const actions = el("div", { className: "status-failed-actions" });
      actions.appendChild(secondaryButton("Dismiss", () => options.onDismiss && options.onDismiss(), { className: "status-failed-dismiss" }));
      actions.appendChild(primaryButton("Retry", () => options.onRetry && options.onRetry(), { className: "status-failed-retry" }));
      box.appendChild(actions);
      return box;
    }

    const badge = el("span", { className: "status-badge status-badge-" + family });
    if (family === "progress") {
      badge.appendChild(el("span", { className: "status-badge-bar" }));
    } else if (family === "live") {
      badge.appendChild(el("span", { className: "status-badge-dot" }));
    } else if (family === "degraded") {
      badge.appendChild(icon("warning", "status-badge-icon"));
    }
    const text = el("span", { className: "status-badge-label", text: label });
    badge.appendChild(text);

    if (family === "degraded" && options.since) {
      let intervalId = null;
      const tick = () => {
        if (!badge.isConnected) {
          clearInterval(intervalId);
          return;
        }
        const elapsedS = Math.max(0, Math.round((Date.now() - options.since) / 1000));
        text.textContent = `${label} · ${elapsedS}s`;
      };
      tick();
      intervalId = setInterval(tick, 1000);
    }

    return badge;
  }

  /** Shared hatch fill with `.status-badge-degraded` (same CSS rule, see
   * status.css) — used by the leave-room confirm dialog's warning banners.
   * Not one of the 4 status families itself (it's a page-level warning
   * banner, not a state pill), just visually related enough to reuse the
   * same amber-hatch treatment rather than inventing a second one. */
  function hatchedBanner(text) {
    return el("div", { className: "hatched-banner" }, [icon("warning", "hatched-banner-icon"), el("span", { className: "hatched-banner-text", text })]);
  }

  /** Lighter, no-badge empty-row for in-context "this list is empty" rows
   * (a room's member list, ...) — icon + text only, no big gradient badge.
   * See `renderEmptyState` below for the whole-panel version. */
  function emptyRow(glyph, text) {
    return el("div", { className: "empty-row" }, [icon(glyph, "empty-row-icon"), el("span", { text })]);
  }

  // -------------------------------------------------------------------
  // Room row — the sidebar ROOMS section's per-room row + expanded member
  // list, per APP-SHELL.md's three-state table.
  // -------------------------------------------------------------------

  /**
   * @param {object} room - a `SavedRoomRow` DTO shape { name, join_string, state:
   *   'joined'|'disconnected'|'reconnecting', room_handle, active_stream_count },
   *   plus two frontend-only arrays populated together via `room_detail` (no
   *   single DTO inlines any of this — see `loadRoomMembers`, shell.js):
   *   `members`: [{conn_id, display_name, owner, stream_id, resolution,
   *   watching}], and `streams`: every announced stream in the room
   *   ([{owner, stream_id, kind, ...}]) — used here only to test "does this
   *   member's `conn_id` own an entry" for the `[LIVE]` badge.
   * @param {object} handlers - { onOpenRoom, onReconnect, onLeave }
   *
   * Per the sidebar-redesign handoff (option 1c): every state renders through
   * ONE fixed row skeleton — `[dot][caret/reconnect][name][meta, right-
   * aligned][action]` — rather than each state inventing its own layout. Only
   * the *content* of the caret/meta/action slots changes per state; the slots
   * themselves are always present (even empty, as a spacer) so columns stay
   * aligned down the whole ROOMS list regardless of which states are mixed
   * in. This replaces the previous two-line "disconnected" layout (Reconnect
   * pill + forget button on a sub-line) — that variance existed only because
   * the old design put a *labeled* Reconnect button next to a
   * possibly-long auto-generated name; the redesign's single reconnect
   * *icon* (↩) in the caret slot is narrow enough to keep everything on one
   * line without crowding the name out.
   *
   * `Why:` (room-detail-slide-over pass, option 1f) a joined row no longer
   * expands inline on click to show its member list — clicking anywhere on
   * a joined row's head (or its caret specifically) now calls
   * `handlers.onOpenRoom` to open the room-detail slide-over panel instead
   * (see shell.js's `openRoomPanel`/`renderRoomPanel`), which owns rename,
   * audio-share and leave actions. Deliberate UX call: the WHOLE row is
   * clickable (not just the caret) since "click a room to see its detail"
   * is the more discoverable target on a dense sidebar list, while the
   * row's own leave/forget icon-button in the action slot stays as a fast
   * path that does NOT open the panel first (its own click handler stops
   * propagation) — a user who already knows they want to leave shouldn't be
   * forced through the panel to do it.
   *
   * `Why:` (2026-07-28, room-row redesign) the row click now opens the
   * panel for EVERY state, not just `joined` — a disconnected/reconnecting
   * row used to be a dead click outside its caret/action icons, which read
   * as broken next to the joined rows' full-row affordance. `openRoomPanel`
   * (shell.js) degrades its content per state (no live roster to show
   * without a `room_handle`) rather than refusing to open. The
   * disconnected row's reconnect action stays a fast path independent of
   * the panel: its caret slot (see below) still stops propagation so
   * clicking it reconnects without first opening/closing the panel.
   *
   * `Why:` (2026-07-28, member/LIVE tree pass) the row's meta slot used to
   * show a `"{count} active"` summary; it's now always shown below the row
   * itself, one line per current member, each carrying a `[LIVE]` pill if
   * that member owns a stream in `room.streams` — the same per-stream
   * roster `renderRoomPanelStreamRow` (shell.js) already reads for the
   * panel's "Live streams" section, reused here rather than re-derived. See
   * `superdoc/features/stream-control.md`'s 2026-07-28 note for the
   * eager-roster-fetch tradeoff this required in `shell.js`'s
   * `refreshRooms`.
   */
  function renderRoomRow(room, handlers) {
    handlers = handlers || {};
    const row = el("div", { className: "room-row room-row-" + room.state });

    const head = el("div", { className: "room-row-head room-row-head-clickable" });
    head.addEventListener("click", () => handlers.onOpenRoom && handlers.onOpenRoom(room.join_string));

    head.appendChild(statusDot(room.state));

    // Caret slot: open the room-detail panel (joined), reconnect
    // (disconnected — now a plain yellow dot rather than the old
    // `restart_alt` glyph, see `.room-caret-reconnect-dot` in shell.css; the
    // arrow read as "pointing at itself" per the design owner's screenshot
    // note), or an empty spacer (reconnecting — "nothing to click while
    // connecting"). Every caret's own click stops propagation so it doesn't
    // double-fire the head's row-wide click handler above (which now opens
    // the panel for every state, not just joined — see this function's doc
    // comment).
    if (room.state === "joined") {
      head.appendChild(
        el("button", {
          className: "room-caret",
          onClick: (e) => {
            e.stopPropagation();
            handlers.onOpenRoom && handlers.onOpenRoom(room.join_string);
          },
          attrs: { "aria-label": "Room details", title: "Room details" },
        }, [icon("chevron_right")])
      );
    } else if (room.state === "disconnected") {
      head.appendChild(
        el("button", {
          className: "room-caret room-caret-reconnect",
          onClick: (e) => {
            e.stopPropagation();
            handlers.onReconnect && handlers.onReconnect(room.join_string);
          },
          attrs: { "aria-label": "Reconnect", title: "Reconnect" },
        }, [el("span", { className: "status-dot room-caret-reconnect-dot" })])
      );
    } else {
      head.appendChild(el("span", { className: "room-caret room-caret-spacer" }));
    }

    head.appendChild(el("span", { className: "room-name", text: room.name }));

    // Meta slot: short, fixed-width, right-aligned status text — never the
    // free-text state body (that would fight the name for width on this
    // monospace, narrow-sidebar layout). Joined rooms used to show a
    // `"{count} active"` summary here; that's now the member/LIVE tree
    // appended below the row instead (see this function's doc comment), so
    // joined falls through to the "—" default.
    // `Why:` (2026-07-28) disconnected used to show a "not connected" meta
    // string here — redundant with the row's own outline status dot (and
    // its name already going dim, `.room-row-disconnected .room-name` in
    // shell.css) per the design owner's screenshot feedback. Falls through
    // to the same "—" default `reconnecting` already used.
    const metaText = "—";
    head.appendChild(el("span", { className: "room-meta", text: metaText }));

    // Action slot: leave (joined), forget (disconnected), or empty (nothing
    // to act on mid-reconnect). Always present so the column stays aligned.
    // Both buttons stop propagation — this stays a fast path independent of
    // the panel, not an implicit "open panel then leave" detour.
    const actionSlot = el("span", { className: "room-action-slot" });
    if (room.state === "joined") {
      actionSlot.appendChild(
        iconButton(
          "close",
          (e) => {
            e.stopPropagation();
            handlers.onLeave && handlers.onLeave(room.join_string);
          },
          { title: "Leave room", className: "room-row-action-btn" }
        )
      );
    } else if (room.state === "disconnected") {
      actionSlot.appendChild(
        iconButton(
          "delete",
          (e) => {
            e.stopPropagation();
            handlers.onLeave && handlers.onLeave(room.join_string);
          },
          { title: "Forget room", className: "room-row-action-btn" }
        )
      );
    }
    head.appendChild(actionSlot);

    row.appendChild(head);

    // Member/LIVE tree — one indented row per current member of a joined
    // room, LIVE-badged if that member owns a stream in `room.streams`
    // (populated by `loadRoomMembers`, same source `renderRoomPanelStreamRow`
    // reads for the panel's "Live streams" section — reused here, not
    // re-derived). Disconnected/reconnecting rooms never have a live roster
    // to show (`room.members` is only populated for joined rooms).
    if (room.state === "joined" && (room.members || []).length > 0) {
      const liveOwners = new Set((room.streams || []).map((s) => s.owner));
      const tree = el("div", { className: "room-member-tree" });
      room.members.forEach((m) => {
        const memberRow = el("div", { className: "room-member-tree-row" });
        memberRow.appendChild(el("span", { className: "room-member-tree-name", text: m.display_name }));
        if (liveOwners.has(m.conn_id)) {
          memberRow.appendChild(
            el("span", { className: "room-member-live-badge" }, [
              el("span", { className: "room-member-live-dot" }),
              el("span", { text: "LIVE" }),
            ])
          );
        }
        tree.appendChild(memberRow);
      });
      row.appendChild(tree);
    }

    return row;
  }

  // -------------------------------------------------------------------
  // Stream row — the sidebar STREAMS section's per-destination row.
  // -------------------------------------------------------------------

  /** Maps `capture::DestinationState`'s variants to the sidebar OUTGOING
   * row's dot color + short meta label — same 4-family status vocabulary as
   * `statusBadge`/`destinationCard` (status.css), just a plain dot instead of
   * a pill (this row is dense enough that a full badge would crowd the
   * bitrate/room-label columns). */
  const OUTGOING_STATE_META = {
    starting: { label: "STARTING", dotClass: "status-dot-progress" },
    rebuilding: { label: "APPLYING", dotClass: "status-dot-progress" },
    live: { label: null, dotClass: "status-dot-live" },
    stalled: { label: "STALLED", dotClass: "status-dot-degraded" },
    failed: { label: "FAILED", dotClass: "status-dot-failed" },
  };

  /**
   * One row per active OUTGOING destination (the sidebar's cross-room
   * "STREAMS"/"OUTGOING" section — see APP-SHELL.md's STREAMS section doc).
   * Follows the same fixed row skeleton as `renderRoomRow`:
   * `[dot][room/destination label][meta, right-aligned]`, plus a "Stop"
   * button that's an absolutely-positioned hover/focus-reveal overlay (not
   * a normal flex child) — it reserves no width at rest, so the label/meta
   * get the row's full space until hover, matching view.css's
   * `.media-tile-overlay` hover-reveal pattern.
   *
   * @param {object} stream - a `DestinationRow` DTO shape { destination_id, kind:
   *   'video'|'audio', state: 'starting'|'live'|'rebuilding'|'stalled'|'failed',
   *   width, height, framerate, bitrate_kbps }, plus a frontend-only `room_label`
   *   (looked up from the room's `SavedRoomRow.name` by `room_handle` —
   *   `DestinationRow` itself only carries the numeric handle, not a display name).
   * @param {object} [handlers] - { onStop(destinationId) } — calls the same real
   *   `stop_destination` command `stream-control.js`'s own Stop button uses; this
   *   is just a lighter-weight sidebar entry point onto that one command, not a
   *   second copy of any stop logic.
   */
  function renderStreamRow(stream, handlers) {
    handlers = handlers || {};
    const meta = OUTGOING_STATE_META[stream.state] || { label: null, dotClass: "status-dot-live" };

    const row = el("div", { className: "stream-row" });
    row.appendChild(el("span", { className: "status-dot " + meta.dotClass }));
    row.appendChild(el("span", { className: "stream-label", text: stream.room_label }));
    const metaText = meta.label || (stream.kind === "audio" ? "" : `${stream.width}×${stream.height} @ ${stream.framerate}fps`);
    row.appendChild(el("span", { className: "stream-meta", text: metaText }));
    if (stream.kind === "audio") row.appendChild(icon("volume_up", "stream-audio-glyph"));
    const stopBtn = dangerButton("Stop", () => handlers.onStop && handlers.onStop(stream.destination_id), {
      className: "stream-stop-overlay-btn",
    });
    stopBtn.prepend(icon("stop_circle"));
    stopBtn.title = "Stop";
    stopBtn.setAttribute("aria-label", "Stop");
    row.appendChild(stopBtn);
    return row;
  }

  /**
   * Standardized whole-panel empty state: icon-in-a-badge + one bold
   * headline + one sentence of body copy + one primary action. Used across
   * every "there's nothing here" panel (no rooms yet, nobody sharing in a
   * room, nobody on screen, no active shares) so they all read as the same
   * visual language rather than each screen inventing its own.
   *
   * Backward-compatible with every existing 2/3-arg call site: without
   * `opts.headline`, `message` itself becomes the (only) headline line, no
   * separate body line — the simplest correct case for a caller that just
   * has one short sentence to say.
   *
   * @param {string} message - body copy (or the headline, if `opts.headline`
   *   is absent).
   * @param {string|null} actionLabel
   * @param {(() => void)|null} onAction
   * @param {object} [opts] - { icon: glyph name, headline: string,
   *   inline: Element (e.g. an inline join-room form), className }
   */
  function renderEmptyState(message, actionLabel, onAction, opts) {
    opts = opts || {};
    const wrap = el("div", { className: "empty-state" + (opts.className ? " " + opts.className : "") });
    wrap.appendChild(el("div", { className: "empty-state-badge" }, [icon(opts.icon || "info", "empty-state-badge-icon")]));
    wrap.appendChild(el("p", { className: "empty-state-headline", text: opts.headline || message }));
    if (opts.headline) {
      wrap.appendChild(el("p", { className: "empty-state-body", text: message }));
    }
    if (opts.inline) wrap.appendChild(opts.inline);
    if (actionLabel) {
      wrap.appendChild(primaryButton(actionLabel, onAction, { className: "empty-state-action" }));
    }
    return wrap;
  }

  // -------------------------------------------------------------------
  // Card — a generic, equal-width section container. Added for the Stream
  // Control / View / Settings tab content: `.tab-placeholder-card` (shell.css)
  // was a one-off for the nav-switcher proof and capped itself at
  // `--panel-max-width`, which is exactly the "content-sized card" shape the
  // Stream Control redesign is fixing. `.card` has no width opinion of its
  // own — the caller's grid/flex container decides equal-width sizing (see
  // `stream-control.css`'s `.sc-grid`), so a card never reverts to shrinking
  // to fit its content.
  // -------------------------------------------------------------------

  /**
   * @param {string|null} label - section-label heading text, or null/'' for no heading.
   * @param {Element[]} children
   * @param {object} [opts] - { className }
   */
  function card(label, children, opts) {
    opts = opts || {};
    const c = el("div", { className: "card" + (opts.className ? " " + opts.className : "") });
    if (label) c.appendChild(el("div", { className: "section-label card-label", text: label }));
    (children || []).forEach((ch) => ch && c.appendChild(ch));
    return c;
  }

  // -------------------------------------------------------------------
  // Warning banner — mirrors `theme::warning_banner` (stream-control.md's
  // bandwidth-overshoot banner: WARNING-stroked, tinted background).
  // -------------------------------------------------------------------

  function warningBanner(text) {
    return el("div", { className: "warning-banner" }, [
      icon("warning", "warning-banner-icon"),
      el("span", { className: "warning-banner-text", text }),
    ]);
  }

  // -------------------------------------------------------------------
  // Dropdown — the custom-styled replacement for native `<select>`. A
  // native select's open popup is platform/OS chrome CSS cannot reach
  // (stark white, generic font, no dark theme, no "roll out" animation, and
  // it looks different per OS/Chromium build) — this rebuilds the same
  // trigger+popup shape entirely out of ordinary elements so it can be
  // themed like every other control and animate open/closed. See
  // superdoc/ui/STYLING-GUIDE.md (2026-07-28) for the full rationale.
  // -------------------------------------------------------------------

  /**
   * @param {{value: *, label: string}[]} options - `value` may be any type;
   *   matching against the current value (and the value handed back to
   *   `onChange`) is done via `String(value)` equality so callers can use
   *   numbers, strings, whatever their option's `value` already was — same
   *   as a native `<select>`, callers that need a number back should
   *   `Number(...)` it in `onChange`, same as they did with `select.value`.
   * @param {*} value - the currently-selected option's `value`
   * @param {(value: *) => void} onChange - fired only on a real user pick,
   *   never for a programmatic `.value = ...` set (matches native `<select>`,
   *   which doesn't fire `change` for scripted assignment either).
   * @param {object} [opts] - { className: string (applied to the trigger
   *   button, so existing sizing classes like `.dest-room-select` keep
   *   their `max-width`/padding/etc.), placeholder: string, emptyText:
   *   string (shown, disabled-looking, when `options` is empty) }
   * @returns {HTMLElement} a `<div class="dd">` wrapper. Beyond being a
   *   normal appendable/removable DOM node, it also exposes: a `.value`
   *   getter/setter (get/set the selection without going through the UI,
   *   like `select.value`), `.setOptions(newOptions)` (rebuild the option
   *   list in place, e.g. for an async-refreshed source list), and
   *   `.focus()` (delegates to the trigger button). The native `.hidden`
   *   property/attribute works as-is since it's a plain element.
   */
  function dropdown(options, value, onChange, opts) {
    opts = opts || {};
    const root = el("div", { className: "dd" });
    const trigger = el("button", {
      className: "dd-trigger" + (opts.className ? " " + opts.className : ""),
      attrs: { type: "button" },
    });
    const labelEl = el("span", { className: "dd-trigger-label" });
    trigger.appendChild(labelEl);
    trigger.appendChild(icon("expand_more", "dd-chevron"));
    root.appendChild(trigger);

    // The option list lives outside `root` in the DOM (appended to <body>
    // only while open, removed again on close) rather than as a plain
    // `position: absolute` child of `root`. Several call sites nest this
    // control inside an `overflow: hidden` ancestor (`.dest-card`'s rounded
    // corners, in particular) — an absolutely-positioned child would be
    // silently clipped there. It's created once and reused across
    // open/close so re-opening doesn't rebuild it from scratch.
    const list = el("div", { className: "dd-list", attrs: { role: "listbox" } });

    let currentOptions = options || [];
    let currentValue = value;
    let open = false;

    function findOption(v) {
      return currentOptions.find((o) => String(o.value) === String(v));
    }

    function renderLabel() {
      const match = findOption(currentValue);
      labelEl.textContent = match ? match.label : opts.placeholder || "";
    }

    function renderOptions() {
      list.innerHTML = "";
      if (currentOptions.length === 0) {
        list.appendChild(el("div", { className: "dd-option-empty", text: opts.emptyText || "No options" }));
        return;
      }
      currentOptions.forEach((o) => {
        const isSelected = String(o.value) === String(currentValue);
        const item = el("button", {
          className: "dd-option" + (isSelected ? " dd-option-selected" : ""),
          attrs: { type: "button", role: "option", "aria-selected": String(isSelected) },
        });
        item.appendChild(el("span", { className: "dd-option-label", text: o.label }));
        if (isSelected) item.appendChild(icon("check", "dd-option-check"));
        item.addEventListener("click", () => {
          currentValue = o.value;
          renderLabel();
          renderOptions();
          closeList();
          root.dispatchEvent(new Event("change"));
          onChange && onChange(o.value);
        });
        list.appendChild(item);
      });
    }

    /** Places `list` (already in <body>) directly under/over the trigger,
     * flipping above it only when there's not enough room below *and*
     * there's more room above — a simple check, not full collision/popover
     * logic (not needed: this app has no nested-scroll-container cases
     * beyond the simple "near the bottom of the window" one). */
    function position() {
      const rect = trigger.getBoundingClientRect();
      list.style.left = rect.left + "px";
      // `min-width`, not `width`: a narrow trigger (e.g. the 76px Framerate
      // picker) would otherwise force every option's label to wrap onto two
      // lines. Options can be wider than the trigger; they always share its
      // left edge.
      list.style.minWidth = rect.width + "px";
      const spaceBelow = window.innerHeight - rect.bottom;
      const spaceAbove = rect.top;
      const wantsFlip = spaceBelow < 160 && spaceAbove > spaceBelow;
      list.classList.toggle("dd-list-up", wantsFlip);
      if (wantsFlip) {
        list.style.bottom = window.innerHeight - rect.top + 4 + "px";
        list.style.top = "";
        list.style.maxHeight = Math.max(120, spaceAbove - 12) + "px";
      } else {
        list.style.top = rect.bottom + 4 + "px";
        list.style.bottom = "";
        list.style.maxHeight = Math.max(120, spaceBelow - 12) + "px";
      }
    }

    function onReposition() {
      if (!trigger.isConnected) {
        closeList();
        return;
      }
      position();
    }

    function onDocMouseDown(e) {
      if (!trigger.isConnected || (!root.contains(e.target) && !list.contains(e.target))) closeList();
    }

    function onKeyDown(e) {
      if (e.key === "Escape") {
        closeList();
        trigger.focus();
      }
    }

    function openList() {
      if (open) return;
      open = true;
      document.body.appendChild(list);
      position();
      // A frame late, same "already in the DOM before the class flips"
      // trick this codebase already uses for its other open/close
      // transitions (see e.g. shell.css's `.room-panel-scrim-open`) — the
      // transition needs the closed (scaleY(0.9)/opacity 0) state to have
      // actually painted first, or the browser coalesces both style
      // changes into one frame and it just appears instead of rolling out.
      requestAnimationFrame(() => list.classList.add("dd-list-open"));
      trigger.classList.add("dd-trigger-open");
      document.addEventListener("mousedown", onDocMouseDown, true);
      document.addEventListener("keydown", onKeyDown, true);
      window.addEventListener("scroll", onReposition, true);
      window.addEventListener("resize", onReposition);
    }

    function closeList() {
      if (!open) return;
      open = false;
      list.classList.remove("dd-list-open", "dd-list-up");
      trigger.classList.remove("dd-trigger-open");
      document.removeEventListener("mousedown", onDocMouseDown, true);
      document.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("scroll", onReposition, true);
      window.removeEventListener("resize", onReposition);
      if (list.parentNode) list.parentNode.removeChild(list);
    }

    trigger.addEventListener("click", () => (open ? closeList() : openList()));

    Object.defineProperty(root, "value", {
      get() {
        return currentValue;
      },
      set(v) {
        currentValue = v;
        renderLabel();
        renderOptions();
      },
    });
    root.setOptions = function (newOptions) {
      currentOptions = newOptions || [];
      renderLabel();
      renderOptions();
    };
    root.focus = function () {
      trigger.focus();
    };

    renderLabel();
    renderOptions();
    return root;
  }

  // -------------------------------------------------------------------
  // Two-option pill switch — mirrors `theme.rs`'s use of a `selectable_label`
  // pair for exclusive two-choice pickers (the bandwidth mode toggle, the
  // Settings tab's language switcher). One shared implementation so both
  // call sites don't drift into near-copies of each other.
  // -------------------------------------------------------------------

  /**
   * @param {{value: string, label: string}[]} options
   * @param {string} value - currently selected option's `value`
   * @param {(value: string) => void} onChange
   */
  function pillSwitch(options, value, onChange) {
    const wrap = el("div", { className: "mode-toggle" });
    options.forEach((opt) => {
      wrap.appendChild(
        el("button", {
          className: "mode-toggle-btn" + (opt.value === value ? " mode-toggle-btn-active" : ""),
          text: opt.label,
          onClick: () => onChange(opt.value),
        })
      );
    });
    return wrap;
  }

  /**
   * Bandwidth-mode-specific convenience wrapper over `pillSwitch`, per
   * stream-control.md's "Absolute Maximum" / "Per-Stream Maximum" naming.
   * Values match `dto::bandwidth_mode_str`'s wire strings exactly
   * ("absolute" / "per_stream", underscore — not the "per-stream" hyphen
   * this file used before it was wired to the real DTO).
   * @param {'absolute'|'per_stream'} mode
   * @param {(mode: 'absolute'|'per_stream') => void} onChange
   */
  function bandwidthModeToggle(mode, onChange) {
    return pillSwitch(
      [
        { value: "absolute", label: "Absolute Maximum" },
        { value: "per_stream", label: "Per-Stream Maximum" },
      ],
      mode,
      onChange
    );
  }

  // -------------------------------------------------------------------
  // Encoder tuning presets — the "Low-Latency / Balanced / Quality" pill
  // switch layered over the raw tune/quality_level/rate_control fields.
  // Shared by `destinationCard`'s per-destination edit panel (a running
  // share) and `stream-control.js`'s "START A STREAM" form (before a share
  // exists), which is why this lives at module scope instead of inside
  // `destinationCard` — both need the exact same three combos rather than
  // two near-copies drifting apart. Values (`low_latency`/`best_quality`/
  // `performance`/`balanced`/`quality`/`constant`) are `src-tauri/src/
  // dto.rs`'s exact `parse_tune_mode`/`parse_quality_level`/
  // `parse_rate_control_mode` wire strings, including `QualityLevel::
  // Balanced`'s `"balanced"`.
  //
  // `Why: "Balanced" as the middle option` — it's the recommended default
  // for a regular user who doesn't know what "rate control" means;
  // "Low-Latency" and "Quality" mirror the two extremes the raw Tune switch
  // already offers, each paired with the Quality Level/Rate Control combo
  // that actually matches that intent.
  const ENCODER_PRESETS = [
    { value: "low_latency", label: "Low-Latency", tune: "low_latency", qualityLevel: "performance", rateControl: "constant" },
    { value: "balanced", label: "Balanced", tune: "best_quality", qualityLevel: "balanced", rateControl: "constant" },
    { value: "quality", label: "Quality", tune: "best_quality", qualityLevel: "quality", rateControl: "quality" },
  ];

  /** Whichever preset's 3 fields exactly match `tune`/`qualityLevel`/
   * `rateControl`, or `null` if none does (an allowed, unhighlighted state —
   * see `destinationCard`'s `activePreset` doc note). */
  function activeEncoderPreset(tune, qualityLevel, rateControl) {
    return ENCODER_PRESETS.find((p) => p.tune === tune && p.qualityLevel === qualityLevel && p.rateControl === rateControl) || null;
  }

  // -------------------------------------------------------------------
  // Destination card — the Stream Control tab's Active Shares entry. One of
  // these per outgoing destination, across every room (cross-room, per
  // APP-SHELL.md's STREAMS-section rationale). Deliberately dumb: it renders
  // whatever fields/state the caller hands it and calls back through
  // `handlers` for every action, so a later worker swapping mock state for
  // real `invoke()` results only needs to change the caller, not this markup.
  // -------------------------------------------------------------------

  /**
   * @param {object} dest - a `DestinationRow` DTO shape { destination_id, capture_id,
   *   room_handle, kind: 'video'|'audio', state: 'starting'|'live'|'rebuilding'|
   *   'stalled'|'failed', width, height, framerate, bitrate_kbps, pending }, plus
   *   frontend-only `room_label` (looked up by `room_handle`), `pendingRoleText`
   *   (derived from `pending`) and `computedShareKbps` (Absolute-mode allocator
   *   display, not part of any DTO).
   * @param {object} handlers - { onEditApply(destinationId, {bitrateKbps, framerate,
   *   tune, qualityLevel, rateControl}), onTransfer(destinationId, roomHandle),
   *   onFanout(captureId, roomHandle), onStop(destinationId),
   *   onAddAudio(dest, source), onAudioMute(captureId, muted),
   *   onAudioStop(audioDestinationId), onRefreshAudioSources() }
   *
   * `kind: 'audio'` rows never reach this function any more: audio is not a
   * stream of its own in this UI, it is a line inside the screen share it
   * belongs to (`opts.audio`). See `stream-control.js`'s
   * `updateActiveSharesCard`, which pairs them up and filters audio rows out
   * of the top-level list.
   * @param {object} [opts] - { roomOptions: [{id,name}], encoderTuningEditable: bool,
   *   perStreamBitrateEditable: bool, framerateOptions: number[],
   *   roomsAlreadyFedByCapture: number[], editState: {bitrateKbps, framerate, tune,
   *   qualityLevel, rateControl, error}, stalledSince: number|null, audio: object|null,
   *   audioSources: array, staged: bool, stagedRoomHandle: number }.
   *
   *   `staged` renders the "Share Screen"/"Share another screen" flow's
   *   pre-"Go live" card instead of a normal live one: `dest` is a
   *   `StagedCaptureRow`-shaped object ({ capture_id, state: 'starting'|
   *   'ready', width, height, framerate, bitrate_kbps } — no destination_id/
   *   room_handle yet), no room/fan-out/transfer/audio affordances are shown,
   *   the edit grid gains a Room picker (reads/writes `stagedRoomHandle` via
   *   `handlers.onStagedRoomChange(captureId, roomHandle)`), and the actions
   *   row is "Go live"/"Cancel" (`handlers.onGoLive(captureId)`/
   *   `handlers.onCancel(captureId)`) instead of Transfer/Stop. See
   *   `superdoc/features/stream-control.md`'s 2026-07-28 note.
   *
   *   `editState` is this destination's local edit buffer for everything
   *   `edit_destination` can set (kept by the caller, since `DestinationRow` has
   *   no fields to read the server's current tune/quality/rate-control back
   *   from). **Every control in this panel renders from `editState`, never from
   *   the `dest` DTO** — see the Bitrate slider's `Why:` note below for the
   *   snap-back bug that rule fixes.
   *
   *   `encoderTuningEditable` and `perStreamBitrateEditable` gate two different
   *   things and are deliberately not one flag: the former shows the whole
   *   panel (Preset / Framerate / Advanced tuning), the latter only decides
   *   whether the Bitrate slider is among its rows. See where each is used.
   *
   *   `stalledSince` is the caller's client-side-tracked "first observed
   *   Stalled at" timestamp (ms epoch) — see `statusBadge`'s doc comment for why
   *   this can't come from the DTO itself. `audio` is this share's paired
   *   audio `DestinationRow` (or null); `audioSources` is the last-fetched
   *   `list_audio_sources` result, used to populate the "+ Add audio"
   *   picker.
   */
  function destinationCard(dest, handlers, opts) {
    handlers = handlers || {};
    opts = opts || {};
    // Maps `capture::DestinationState`'s real variants (src/capture.rs) to
    // one of the 4 status-vocabulary families (status.css) plus its label.
    const STATE_META = {
      starting: { label: "STARTING", family: "progress" },
      // Staged capture, portal/pipeline up and "Go live" ready to press — see
      // `opts.staged`'s doc note below and `dto::StagedCaptureRow`.
      ready: { label: "READY", family: "progress" },
      rebuilding: { label: "APPLYING", family: "progress" },
      live: { label: "LIVE", family: "live" },
      stalled: { label: "WAITING FOR CAPTURE", family: "degraded" },
      failed: { label: dest.room_label ? `${dest.room_label} stream failed` : "Stream failed", family: "failed" },
    };
    const meta = STATE_META[dest.state] || { label: dest.state, family: "progress" };

    // `dest-card-<state>` also drives the 4px left accent bar (stream-control.css's
    // `.dest-card::before` family) — solid green for live, an animated
    // diagonal stripe for starting/rebuilding (progress), the shared amber
    // hatch for stalled (degraded), reusing status.css's exact status-
    // vocabulary treatments rather than inventing new colors/animations.
    const wrap = el("div", { className: "dest-card dest-card-" + dest.state });

    // Failed is a squared card, not a pill (status.css's `.status-failed-box`
    // header note) — its own compact layout rather than the meta/edit/actions
    // stack every other state gets, since none of that applies to a
    // destination producing no frames. No preview thumbnail either — there's
    // nothing live to show.
    if (meta.family === "failed") {
      const head = el("div", { className: "dest-card-head" });
      head.appendChild(el("span", { className: "dest-room-label", text: dest.room_label }));
      wrap.appendChild(head);
      wrap.appendChild(
        statusBadge("failed", meta.label, {
          errorText: dest.errorText,
          onRetry: () => handlers.onRetry && handlers.onRetry(dest),
          onDismiss: () => handlers.onStop && handlers.onStop(dest.destination_id),
        })
      );
      return wrap;
    }

    // `Why there is no preview thumbnail here:` there used to be a
    // `.dest-thumb` column — a static icon/gradient placeholder, never a
    // live decode (a real self-preview would have needed the sharer's own
    // `conn_id` and the capture's video `stream_id` to call `toggle_watch`/
    // `stream_video_url` against, neither of which any Tauri command
    // exposed). The owner confirmed it was useless precisely because it
    // never showed the actual stream and asked for it to be removed outright
    // — see `superdoc/features/stream-control.md`'s 2026-07-28 note — so the
    // card is now meta/edit/actions only, live or staged, with no preview
    // column at all.
    const body = el("div", { className: "dest-card-body" });

    const head = el("div", { className: "dest-card-head" });
    // Staged: no room chosen yet (that's what the Room field in the edit grid
    // below is for), so the head just names the card generically instead of
    // a room it doesn't have.
    head.appendChild(el("span", { className: "dest-room-label", text: opts.staged ? "Screen Share" : dest.room_label }));
    head.appendChild(statusBadge(meta.family, meta.label, meta.family === "degraded" ? { since: opts.stalledSince } : {}));
    body.appendChild(head);

    if (dest.pendingRoleText) {
      body.appendChild(el("div", { className: "dest-pending-role", text: dest.pendingRoleText }));
    }

    // Metric line + inline sparkline. The metric line is the existing real
    // resolution/framerate text; the sparkline (6 bars) is populated
    // out-of-band by `stream-control.js`'s `updateStats()`, same as the FPS
    // readout right below — no historical stats are fetched here, just a
    // fixed-size row of bar elements for that function to write into.
    // The prototype breaks these numbers out as LABELLED columns in a sunken
    // inset — RESOLUTION / FRAMERATE / BITRATE, each a micro-caps label over
    // a tabular value, sparkline pushed to the right end. That
    // labelled-instrument look is a good part of what the owner meant by
    // "futuristic"; the port had collapsed it to one unlabelled string
    // ("1920x1080 @ 30fps"), which read as a stray caption once the strip
    // gained its inset styling.
    const metaRow = el("div", { className: "dest-meta-row" });

    const metric = (label, value, extraValueClass) =>
      el("div", { className: "dest-metric" }, [
        el("span", { className: "dest-metric-label", text: label }),
        el("span", { className: extraValueClass ? `dest-metric-value ${extraValueClass}` : "dest-metric-value", text: value }),
      ]);

    metaRow.appendChild(metric("Resolution", `${dest.width}×${dest.height}`));
    metaRow.appendChild(metric("Framerate", `${dest.framerate} fps`));
    // `dest-metric-bitrate-value`: a dedicated class so `stream-control.js`'s
    // `updateStats()` can write straight into this node on every
    // `stream-stats-updated` tick, the same direct-DOM-write path already
    // used for `.dest-fps-readout`/the sparkline — see that function's `Why:`
    // note for why this field specifically needs a live-refreshed value
    // rather than the `dest.bitrate_kbps` this initial render reads (that
    // cached copy goes stale the moment Absolute-Maximum mode's allocator
    // changes this destination's live bitrate, since a tier-1 encoder
    // property write never fires an event that would make `stream-
    // control.js` refetch it).
    metaRow.appendChild(metric("Bitrate", `${dest.bitrate_kbps} kbps`, "dest-metric-bitrate-value"));

    // Kept (now empty) so anything still keying off `.dest-meta` finds it;
    // the visible text moved into the metric columns above. Also acts as
    // the flex spacer that pushes the sparkline to the right edge.
    metaRow.appendChild(el("div", { className: "dest-meta" }));

    const sparkline = el("div", { className: "dest-sparkline", attrs: { hidden: "" } });
    for (let i = 0; i < 6; i++) sparkline.appendChild(el("span", { className: "spark-bar spark-bar-empty" }));
    metaRow.appendChild(sparkline);
    body.appendChild(metaRow);

    // Live captured/sent FPS readout — populated out-of-band by
    // `stream-control.js`'s `updateStats()` on every `stream-stats-updated`
    // tick (every ~250ms), via direct `textContent` writes rather than going
    // through this card's normal sig-diffed render path, so a stats tick
    // never triggers a full card rebuild. Starts hidden until the first tick
    // arrives for this destination.
    body.appendChild(el("div", { className: "dest-fps-readout", attrs: { hidden: "" } }));

    // "Which rooms" chip row — this capture's already-real multi-destination
    // fan-out (`opts.roomsAlreadyFedByCapture`, computed by `stream-control.js`
    // from the live destination list) rendered as small chips, plus a dashed
    // "Also send to…" chip that reveals the same fan-out picker the actions
    // row used to always show inline — restyled into a chip, not rebuilt.
    //
    // Staged: none of this applies — there is no room yet (that's what
    // "Go live" picks), so the row is skipped entirely rather than showing
    // fan-out affordances for a destination that doesn't exist.
    if (!opts.staged) {
      const fedRoomIds = opts.roomsAlreadyFedByCapture && opts.roomsAlreadyFedByCapture.length ? opts.roomsAlreadyFedByCapture : [dest.room_handle];
      const chipRow = el("div", { className: "dest-chip-row" });
      fedRoomIds.forEach((rid) => {
        const label = rid === dest.room_handle ? dest.room_label : (((opts.roomOptions || []).find((r) => r.id === rid) || {}).name || String(rid));
        chipRow.appendChild(el("span", { className: "dest-chip", text: label }));
      });
      const fanoutOptions = (opts.roomOptions || []).filter((r) => r.id !== dest.room_handle && !fedRoomIds.includes(r.id));
      if (fanoutOptions.length > 0) {
        const addWrap = el("span", { className: "dest-chip-inline-add" });
        const fanoutSelect = dropdown(
          fanoutOptions.map((r) => ({ value: r.id, label: r.name })),
          fanoutOptions[0].id,
          (value) => {
            handlers.onFanout && handlers.onFanout(dest.capture_id, Number(value));
            addBtn.hidden = false;
            fanoutSelect.hidden = true;
          },
          { className: "dest-room-select dest-chip-select" }
        );
        fanoutSelect.hidden = true;
        const addBtn = el("button", {
          className: "dest-chip-add-btn",
          text: "+ Also send to…",
          onClick: () => {
            addBtn.hidden = true;
            fanoutSelect.hidden = false;
            fanoutSelect.focus();
          },
        });
        addWrap.appendChild(addBtn);
        addWrap.appendChild(fanoutSelect);
        chipRow.appendChild(addWrap);
      }
      body.appendChild(chipRow);
    }

    // The per-destination encoder edit panel: Preset + Framerate always
    // visible, Tune/Quality Level/Rate Control behind the Advanced
    // disclosure, and — in Per-Stream mode only — a Bitrate slider.
    //
    // `Why this is gated on `encoderTuningEditable` and not on
    // `perStreamBitrateEditable`:` those are two different questions, and
    // conflating them into the single bitrate flag is what used to hide the
    // whole panel in Absolute-Maximum mode. Preset/Framerate/Tune/Quality/
    // Rate Control have nothing to do with the bandwidth allocator — it owns
    // *bitrate*, and only bitrate — so they are just as meaningful, and just
    // as safe to apply, in either bandwidth mode. Only the Bitrate slider
    // itself has to stay Per-Stream-only (see its own note below).
    if (opts.encoderTuningEditable) {
      const editState = opts.editState || { tune: "best_quality", qualityLevel: "balanced", rateControl: "constant" };
      // Staged `dest` objects (`opts.staged`) have no `destination_id` yet —
      // key on `capture_id` instead, same id `handlers.onGoLive`/`onCancel`
      // already use for a staged card.
      const editId = opts.staged ? dest.capture_id : dest.destination_id;
      const applyTuning = (patch) => handlers.onEditApply && handlers.onEditApply(editId, patch);

      // Simple preset selector — a higher-level "Low-Latency / Balanced /
      // Quality" pill-switch layered over the raw tune/quality_level/
      // rate_control fields below, per stream-control.md's simple/advanced
      // split. Picking a preset applies all three underlying fields
      // together through the same `onEditApply`/`edit_destination` path as
      // hand-tuning them individually in the Advanced section below — this
      // is a convenience mapping onto those same three fields, not a fourth
      // field of its own, so nothing changes server-side. See module-scope
      // `ENCODER_PRESETS`' doc comment above (shared with the "START A
      // STREAM" form in `stream-control.js`).
      const PRESETS = ENCODER_PRESETS;
      // Highlight whichever preset's 3 fields exactly match the current
      // edit state; if the Advanced section was hand-tuned to some other
      // combo, none of the 3 preset buttons shows as selected — no
      // validation/blocking, that's an intentional, allowed state.
      const activePreset = activeEncoderPreset(editState.tune, editState.qualityLevel, editState.rateControl);

      // Preset + Bitrate share one grid (`.dest-edit-grid`) so their
      // controls start at the same X regardless of "Preset" vs.
      // "Bitrate (kbps)" label width — previously each row was an
      // independent flex container, so a row's control started wherever
      // its own label happened to end and the group read as ragged.
      // `.dest-edit-row` is `display: contents`, so each row must
      // contribute exactly three children (label / control / value) or
      // grid auto-placement spills the next row's label into the value
      // column — the preset switch has no value readout, so its third
      // cell is an empty spacer, same trick the Advanced rows below use.
      const editGrid = el("div", { className: "dest-edit-grid" });

      // Staged only: which room "Go live" will send into — the one field a
      // live destination's edit panel never shows (a live destination
      // already has a room; "Transfer"/"Also send to…" are what move/add
      // rooms afterwards). `opts.roomOptions` is the same joined-rooms list
      // the live card's Transfer picker already uses.
      if (opts.staged) {
        const roomRow = el("div", { className: "dest-edit-row" });
        roomRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Room" }));
        roomRow.appendChild(
          dropdown(
            (opts.roomOptions || []).map((r) => ({ value: r.id, label: r.name })),
            opts.stagedRoomHandle,
            (value) => handlers.onStagedRoomChange && handlers.onStagedRoomChange(dest.capture_id, Number(value)),
            { className: "dest-room-select" }
          )
        );
        roomRow.appendChild(el("span"));
        editGrid.appendChild(roomRow);
      }

      const presetRow = el("div", { className: "dest-edit-row" });
      presetRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Preset" }));
      presetRow.appendChild(
        pillSwitch(
          PRESETS.map((p) => ({ value: p.value, label: p.label })),
          activePreset ? activePreset.value : null,
          (value) => {
            const preset = PRESETS.find((p) => p.value === value);
            if (preset) applyTuning({ tune: preset.tune, qualityLevel: preset.qualityLevel, rateControl: preset.rateControl });
          }
        )
      );
      presetRow.appendChild(el("span"));
      editGrid.appendChild(presetRow);

      // Framerate — a regular-user setting, so it lives up here with Preset
      // and Bitrate rather than behind the Advanced disclosure with the raw
      // Tune/Rate Control knobs. Until now this tab had no framerate control
      // at all: `edit_destination` has always taken one, and the caller has
      // always sent one, but only ever the value the destination already had,
      // so there was no way to change a running stream's framerate short of
      // stopping and re-sharing it.
      //
      // `Why a <select> and not the pillSwitch the Preset row uses:` the
      // offered range is the same nine values the "Start a Stream" form
      // offers (15…280) — nine pills would not fit this column, and matching
      // that form's own control keeps "the framerate picker" one recognisable
      // thing in both places. `framerateOptions` carries the current value
      // even when it isn't one of the nine, so this never silently misreports
      // what the destination is actually running at.
      const framerateRow = el("div", { className: "dest-edit-row" });
      framerateRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Framerate" }));
      const framerateSelect = dropdown(
        (opts.framerateOptions || [editState.framerate]).map((fps) => ({ value: String(fps), label: fps + " fps" })),
        String(editState.framerate),
        (value) => applyTuning({ framerate: Number(value) }),
        { className: "dest-room-select dest-framerate-select" }
      );
      framerateRow.appendChild(framerateSelect);
      framerateRow.appendChild(el("span"));
      editGrid.appendChild(framerateRow);

      // Bitrate stays Per-Stream-only, unlike everything else in this panel:
      // Absolute mode's allocator owns the live bitrate, so a manual field
      // there would be a control whose input is silently overwritten within
      // the second (per stream-control.md's "Absolute mode ... allocator
      // writes live bitrates straight into the encoders without ever touching
      // the manual values").
      if (opts.perStreamBitrateEditable) {
        const editRow = el("div", { className: "dest-edit-row" });
        editRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Bitrate (kbps)" }));
        // `Why the slider reads `editState.bitrateKbps` and not
        // `dest.bitrate_kbps` — the "changing the bitrate just resets the
        // slider" bug:` this card is rebuilt from scratch whenever its sig
        // changes, and `editState` is *in* that sig — so the moment a drag
        // applies, the caller re-renders and this slider is constructed
        // afresh. Built from the DTO it was constructed from the value the
        // *server* last reported, which is still the old one: the backend
        // change had only just been requested. Worse, it stays old forever
        // for a bitrate-only edit — that's a tier-1 live encoder property
        // write, so the destination never leaves `live`, so
        // `destination-state-changed` never fires, so nothing ever re-fetches
        // `DestinationRow`. The slider therefore snapped back to the previous
        // value and stayed there while the real bitrate really had changed.
        // The edit buffer is the value the user actually chose, so it is what
        // this control renders.
        const slider = el("input", {
          className: "dest-bitrate-slider",
          attrs: { type: "range", min: "200", max: "40000", step: "100", value: String(editState.bitrateKbps) },
        });
        const value = el("span", { className: "dest-bitrate-value", text: editState.bitrateKbps + " kbps" });
        slider.addEventListener("input", () => {
          value.textContent = slider.value + " kbps";
        });
        slider.addEventListener("change", () => {
          applyTuning({ bitrateKbps: Number(slider.value) });
        });
        attachWheelStep(slider);
        editRow.appendChild(slider);
        editRow.appendChild(value);
        editGrid.appendChild(editRow);
      }
      body.appendChild(editGrid);

      // Three testing-oriented encoder tuning knobs (`edit_destination`'s
      // `tune`/`quality_level`/`rate_control`) — same `pillSwitch` widget as
      // the Absolute/Per-Stream bandwidth-mode toggle, one row each since a
      // three-way row (Rate Control) wouldn't fit next to a two-way one
      // cleanly. Each button click applies immediately, matching the
      // Bitrate slider's auto-apply-on-change behavior above (there's no
      // separate combined "Apply" action in this panel).
      //
      // `Why:` behind `<details>` — these three switches are the panel's
      // "testing-oriented" knobs (per this file's own doc comment above),
      // not something a regular user needs to see to just start a stream.
      // Per stream-control.md's simple/advanced split, only the preset
      // selector and Bitrate slider above stay always-visible;
      // Tune/Quality Level/Rate Control move behind a native
      // `<details>/<summary>` disclosure, collapsed by default. `<details>`
      // was picked over hand-rolled show/hide state because (a) this
      // component library has no existing toggle primitive that fits
      // (`pillSwitch` is an exclusive-choice picker, not a disclosure), and
      // (b) each destination card creates its own `<details>` element
      // here, so expand/collapse state is naturally per-card (no shared/
      // global open flag to manage) and needs no JS wiring beyond the
      // browser's own native behavior — same instinct as this file's
      // "prefer the platform" pattern elsewhere (e.g. relying on
      // `<select>`'s native change event rather than a custom dropdown).
      // `opts.advancedOpen` — this card rebuilds from scratch (a fresh
      // `<details>` node) on every edit, per `stream-control.js`'s sig-diffed
      // render (any editState change, including a click inside Advanced
      // itself, invalidates the sig and swaps in a whole new card element).
      // A brand-new `<details>` always starts closed, so without this the
      // panel would visibly snap shut right after the very click that used
      // it (e.g. picking a preset, or flipping Rate Control) — the caller
      // reads the outgoing card's current open/closed state before
      // replacing it and threads it back in here so the disclosure survives
      // its own re-render, same "caller keeps the state" pattern this file
      // already uses for `editState` itself.
      const advanced = el("details", { className: "dest-advanced", attrs: opts.advancedOpen ? { open: "" } : {} });
      advanced.appendChild(el("summary", { className: "dest-advanced-summary", text: "Advanced tuning" }));
      const advancedBody = el("div", { className: "dest-advanced-body" });
      // Own nested grid (not `editGrid` above — that one's already closed
      // out into `body`) so Tune/Quality Level/Rate Control line up with
      // each other the same way Preset/Bitrate do above. `-nested` drops
      // `.dest-edit-grid`'s own top border/padding, since `.dest-advanced-
      // body` already draws that separator from the "Advanced tuning"
      // summary above it — see stream-control.css.
      const advancedGrid = el("div", { className: "dest-edit-grid dest-edit-grid-nested" });

      const tuneRow = el("div", { className: "dest-edit-row" });
      tuneRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Tune" }));
      tuneRow.appendChild(
        pillSwitch(
          [
            { value: "low_latency", label: "Low Latency" },
            { value: "best_quality", label: "Best Quality" },
          ],
          editState.tune,
          (value) => applyTuning({ tune: value })
        )
      );
      // Empty value cell: `.dest-edit-row` is `display: contents`, so each
      // row must contribute exactly three children or grid auto-placement
      // spills the next row's label into the value column.
      tuneRow.appendChild(el("span"));
      advancedGrid.appendChild(tuneRow);

      const qualityRow = el("div", { className: "dest-edit-row" });
      qualityRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Quality Level" }));
      qualityRow.appendChild(
        pillSwitch(
          [
            { value: "performance", label: "Performance" },
            { value: "balanced", label: "Balanced" },
            { value: "quality", label: "Quality" },
          ],
          editState.qualityLevel,
          (value) => applyTuning({ qualityLevel: value })
        )
      );
      qualityRow.appendChild(el("span"));
      advancedGrid.appendChild(qualityRow);

      const rateControlRow = el("div", { className: "dest-edit-row" });
      rateControlRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Rate Control" }));
      rateControlRow.appendChild(
        pillSwitch(
          [
            { value: "constant", label: "Constant" },
            { value: "adaptive", label: "Adaptive" },
            { value: "quality", label: "Quality" },
          ],
          editState.rateControl,
          (value) => applyTuning({ rateControl: value })
        )
      );
      rateControlRow.appendChild(el("span"));
      advancedGrid.appendChild(rateControlRow);

      advancedBody.appendChild(advancedGrid);
      advanced.appendChild(advancedBody);
      body.appendChild(advanced);

      // Error stays outside (and below) the `<details>`, not nested inside
      // it — `applyDestinationEdit` sets `editState.error` for a failed
      // apply of *any* control in this panel, including the always-visible
      // Bitrate slider above, so it needs to be visible regardless of the
      // advanced section's collapsed/expanded state.
      if (editState.error) {
        body.appendChild(el("div", { className: "dest-edit-error", text: editState.error }));
      }
    }

    // The allocator's read-only counterpart to the Bitrate slider: in
    // Absolute-Maximum mode this destination's share is computed, not chosen,
    // so it's reported rather than offered as a control. Sits outside the
    // panel above because it is not gated on the same condition — the panel
    // now renders in *both* bandwidth modes, and this line belongs to
    // Absolute mode alone.
    if (dest.computedShareKbps !== undefined) {
      body.appendChild(el("div", { className: "dest-computed-share", text: `Allocated: ${dest.computedShareKbps} kbps` }));
    }

    // Staged: `dest-actions-staged` bumps `.dest-action-btn` up to a bigger,
    // more deliberate size than the routine small action buttons live cards
    // use (Transfer/Stop) — "Go live" is the one confirming action on the
    // whole card, not a routine icon-sized button, so it (and its "Cancel"
    // companion) need to read as visibly more prominent. See
    // `.dest-actions-staged .dest-action-btn` in stream-control.css.
    const actions = el("div", { className: opts.staged ? "dest-actions dest-actions-staged" : "dest-actions" });

    if (opts.staged) {
      // "Go live" in place of the Transfer picker + Stop button a live card
      // gets: this card has no destination yet, just a running capture and
      // the room picked above, so there is nothing to transfer and "Cancel"
      // (not "Stop") is the honest verb for tearing down a capture that was
      // never actually sent anywhere. Disabled — rather than hidden — while
      // the capture is still `starting`, so the button doesn't visibly jump
      // around the row the instant the portal/pipeline comes up.
      const goLiveBtn = primaryButton("Go live", () => handlers.onGoLive && handlers.onGoLive(dest.capture_id), {
        className: "dest-action-btn dest-go-live-btn",
      });
      if (dest.state !== "ready") goLiveBtn.disabled = true;
      actions.appendChild(goLiveBtn);
      actions.appendChild(
        dangerButton("Cancel", () => handlers.onCancel && handlers.onCancel(dest.capture_id), { className: "dest-action-btn" })
      );
    } else {
      // Transfer only here — fan-out ("Also send to…") moved into the chip
      // row above (`chipRow`'s dashed chip), same `handlers.onFanout` call,
      // just a restyled entry point instead of a second copy of the picker.
      const roomOptions = (opts.roomOptions || []).filter((r) => r.id !== dest.room_handle);
      if (roomOptions.length > 0) {
        const transferSelect = dropdown(
          roomOptions.map((r) => ({ value: r.id, label: r.name })),
          roomOptions[0].id,
          null,
          { className: "dest-room-select" }
        );
        actions.appendChild(transferSelect);
        actions.appendChild(
          secondaryButton("Transfer", () => handlers.onTransfer && handlers.onTransfer(dest.destination_id, Number(transferSelect.value)), {
            className: "dest-action-btn",
          })
        );
      }

      actions.appendChild(dangerButton("Stop", () => handlers.onStop && handlers.onStop(dest.destination_id), { className: "dest-action-btn" }));
    }
    body.appendChild(actions);

    // Audio sub-row — the share's audio, attached to the share rather than
    // living in a card (or a whole separate "SHARE AUDIO" panel) of its own.
    // Audio is not a second stream the user manages; it is a property of the
    // screen share, so it renders as one extra line under a hairline rule.
    // Video-only: an audio destination never renders a card, so it never
    // reaches this branch (see `stream-control.js`'s `updateActiveSharesCard`).
    // Also skipped while staged: audio attaches to a *room* (`start_audio_
    // share`'s `room_handle`), and a staged card doesn't have one until
    // "Go live" picks it.
    if (dest.kind !== "audio" && !opts.staged) {
      body.appendChild(el("div", { className: "dest-divider" }));
      body.appendChild(opts.audio ? audioAttachedRow(opts.audio, handlers) : addAudioRow(dest, handlers, opts));
    }

    wrap.appendChild(body);
    return wrap;
  }

  /**
   * The attached-audio line: `[meter] [app name] [Mute/Unmute] [Stop]`.
   *
   * @param {object} audio - the paired audio `DestinationRow` (its `label` is
   *   the captured application's name — `session::start_audio_share` sets the
   *   capture's label to the picked source's name — and `muted` is the
   *   toggle's current state).
   * @param {object} handlers - { onAudioMute(captureId, muted), onAudioStop(destinationId) }
   */
  function audioAttachedRow(audio, handlers) {
    const row = el("div", { className: "dest-audio-row" });

    // Meter: fixed bar count, animated out-of-band by `stream-control.js`'s
    // `updateStats()` off the `audio_level` field on the already-flowing
    // `stream-stats-updated` tick — same direct-DOM-write approach as the
    // FPS readout and sparkline, so a level change never rebuilds the card.
    // `data-audio-destination-id` is how that function finds this meter from
    // a stats payload keyed by the *audio* destination id, given the meter
    // lives inside the *video* destination's card.
    const meter = el("div", {
      className: "dest-audio-meter",
      attrs: { "data-audio-destination-id": String(audio.destination_id), "aria-hidden": "true" },
    });
    for (let i = 0; i < AUDIO_METER_BARS; i++) meter.appendChild(el("span", { className: "dest-audio-meter-bar" }));
    row.appendChild(meter);

    row.appendChild(el("span", { className: "dest-audio-name", text: audio.label, attrs: { title: audio.label } }));

    // `Why a fixed width:` the label flips between "Mute" and "Unmute", and
    // a button that resizes on every toggle shoves the Stop button next to
    // it sideways — the owner called this out explicitly. The width is
    // reserved in CSS (`.dest-audio-mute`, sized for the longer word) rather
    // than measured here, so it holds regardless of font loading.
    const muted = !!audio.muted;
    row.appendChild(
      secondaryButton(muted ? "Unmute" : "Mute", () => handlers.onAudioMute && handlers.onAudioMute(audio.capture_id, !muted), {
        className: "dest-audio-mute" + (muted ? " dest-audio-mute-on" : ""),
      })
    );

    row.appendChild(
      dangerButton("Stop", () => handlers.onAudioStop && handlers.onAudioStop(audio.destination_id), { className: "dest-audio-stop" })
    );

    return row;
  }

  /** How many bars the attached-audio VU meter draws. */
  const AUDIO_METER_BARS = 5;

  /**
   * The no-audio-yet line: a dashed "+ Add audio" affordance that expands in
   * place into the source picker, reusing the same inline-reveal pattern as
   * the card's "+ Also send to…" fan-out chip rather than opening a dialog
   * or sending the user back to a separate panel.
   *
   * @param {object} opts - { audioSources: [{id,name}], onRefreshAudioSources }
   */
  function addAudioRow(dest, handlers, opts) {
    const row = el("div", { className: "dest-audio-row dest-audio-row-empty" });

    // The list this picker is currently showing. Held in a local rather than
    // re-read from `opts.audioSources` on every use, because
    // `onRefreshAudioSources` *replaces* the caller's array (it reassigns
    // `mockAudioSources = await fetchAudioSources()`), so `opts.audioSources`
    // keeps pointing at whichever array existed when this card was built.
    // Reproduced live in the real app: the picker offered the mock
    // placeholder sources ("Default speakers", "Game (app audio)") instead of
    // the running applications `list_audio_sources` had just returned.
    let sources = opts.audioSources || [];

    const picker = el("span", { className: "dest-audio-picker", attrs: { hidden: "" } });
    const optionsFor = () =>
      sources.length === 0 ? [{ value: "", label: "No application is playing audio" }] : sources.map((s) => ({ value: s.id, label: s.name }));
    const select = dropdown(optionsFor(), sources.length ? sources[0].id : "", null, { className: "dest-room-select dest-audio-select" });
    const rebuildOptions = () => select.setOptions(optionsFor());
    picker.appendChild(select);

    // While the picker stays open, which applications are producing audio
    // can keep changing (an app can start playing well after the picker was
    // opened) — poll `list_audio_sources` again periodically so the list
    // catches up without the user having to close and reopen the picker.
    // Owns its own `setInterval` and stops itself once the picker is no
    // longer visible/attached, same self-stopping `isConnected` pattern
    // `statusBadge`'s degraded-elapsed-time ticker uses above (this
    // codebase's render paths have no uniform unmount hook to call a
    // `.destroy()` from instead).
    const AUDIO_SOURCE_POLL_MS = 5000;
    let audioPollId = null;
    function stopAudioSourcePolling() {
      if (audioPollId !== null) {
        clearInterval(audioPollId);
        audioPollId = null;
      }
    }
    function startAudioSourcePolling() {
      if (audioPollId !== null || !handlers.onRefreshAudioSources) return;
      audioPollId = setInterval(async () => {
        if (!picker.isConnected || picker.hidden) {
          stopAudioSourcePolling();
          return;
        }
        const fresh = await handlers.onRefreshAudioSources();
        if (!picker.isConnected || picker.hidden) {
          // The picker could have closed (or the whole card been rebuilt
          // out from under it) while the await above was in flight.
          stopAudioSourcePolling();
          return;
        }
        if (Array.isArray(fresh)) {
          sources = fresh;
          const previousValue = select.value;
          rebuildOptions();
          if (sources.some((s) => String(s.id) === previousValue)) select.value = previousValue;
        }
      }, AUDIO_SOURCE_POLL_MS);
    }

    picker.appendChild(
      primaryButton("Share", () => {
        const source = sources.find((s) => String(s.id) === select.value);
        if (source) handlers.onAddAudio && handlers.onAddAudio(dest, source);
        stopAudioSourcePolling();
      }, { className: "dest-audio-share-btn" })
    );

    const addBtn = el("button", {
      className: "dest-chip-add-btn dest-audio-add-btn",
      text: "+ Add audio",
      onClick: async () => {
        // Re-list on open: which applications are producing audio changes
        // constantly, so a list captured at card-build time is stale by
        // definition. Awaiting here (rather than pre-fetching every render)
        // also keeps `list_audio_sources`' real system-audio query off the
        // render path.
        if (handlers.onRefreshAudioSources) {
          const fresh = await handlers.onRefreshAudioSources();
          if (Array.isArray(fresh)) sources = fresh;
        }
        rebuildOptions();
        addBtn.hidden = true;
        picker.hidden = false;
        select.focus();
        startAudioSourcePolling();
      },
    });
    row.appendChild(icon("volume_off", "dest-audio-empty-icon"));
    row.appendChild(el("span", { className: "dest-audio-empty-text", text: "No audio attached" }));
    row.appendChild(addBtn);
    row.appendChild(picker);
    return row;
  }

  window.Components = {
    el,
    icon,
    attachWheelStep,
    primaryButton,
    secondaryButton,
    dangerButton,
    iconButton,
    renderNavButton,
    statusDot,
    statusBadge,
    hatchedBanner,
    emptyRow,
    renderRoomRow,
    renderStreamRow,
    renderEmptyState,
    card,
    warningBanner,
    dropdown,
    pillSwitch,
    bandwidthModeToggle,
    destinationCard,
    ENCODER_PRESETS,
    activeEncoderPreset,
  };
})();
