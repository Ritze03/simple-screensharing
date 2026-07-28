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
   *   plus a frontend-only `members` array (fetched separately via `room_detail` —
   *   no single DTO inlines both): [{conn_id, display_name, owner, stream_id,
   *   resolution, watching}].
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
   * expands inline to show its member list — clicking anywhere on a joined
   * row's head (or its caret specifically) now calls `handlers.onOpenRoom`
   * to open the room-detail slide-over panel instead (see shell.js's
   * `openRoomPanel`/`renderRoomPanel`), which owns the member list, rename,
   * audio-share and leave actions this row used to spill into an inline
   * `.room-members` block. Deliberate UX call: the WHOLE row is clickable
   * (not just the caret) since "click a room to see its detail" is the more
   * discoverable target on a dense sidebar list, while the row's own
   * leave/forget icon-button in the action slot stays as a fast path that
   * does NOT open the panel first (its own click handler stops propagation)
   * — a user who already knows they want to leave shouldn't be forced
   * through the panel to do it. Disconnected/reconnecting rows never open
   * the panel (there's no live `room_handle`/roster to show for a room
   * that's not joined) — `onOpenRoom` is only wired for `state === "joined"`.
   */
  function renderRoomRow(room, handlers) {
    handlers = handlers || {};
    const row = el("div", { className: "room-row room-row-" + room.state });

    const head = el("div", { className: "room-row-head" });
    if (room.state === "joined") {
      head.classList.add("room-row-head-clickable");
      head.addEventListener("click", () => handlers.onOpenRoom && handlers.onOpenRoom(room.join_string));
    }

    head.appendChild(statusDot(room.state));

    // Caret slot: open the room-detail panel (joined), reconnect
    // (disconnected), or an empty spacer (reconnecting — "nothing to click
    // while connecting"). The caret's own click stops propagation so it
    // doesn't double-fire the head's row-wide click handler above.
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
          onClick: () => handlers.onReconnect && handlers.onReconnect(room.join_string),
          attrs: { "aria-label": "Reconnect", title: "Reconnect" },
        }, [icon("restart_alt")])
      );
    } else {
      head.appendChild(el("span", { className: "room-caret room-caret-spacer" }));
    }

    head.appendChild(el("span", { className: "room-name", text: room.name }));

    // Meta slot: short, fixed-width, right-aligned status text — never the
    // free-text state body (that would fight the name for width on this
    // monospace, narrow-sidebar layout).
    let metaText = "—";
    if (room.state === "joined") metaText = room.active_stream_count + " active";
    else if (room.state === "disconnected") metaText = "not connected";
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
   * `[dot][room/destination label][meta, right-aligned][stop, hover-reveal]`.
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
    if (stream.kind === "audio") row.appendChild(icon("volume_up", "stream-audio-glyph"));
    row.appendChild(el("span", { className: "stream-label", text: stream.room_label }));
    const metaText = meta.label || (stream.bitrate_kbps ? `${stream.bitrate_kbps} kbps` : `${stream.width}x${stream.height}@${stream.framerate}`);
    row.appendChild(el("span", { className: "stream-meta", text: metaText }));
    row.appendChild(
      iconButton("stop_circle", () => handlers.onStop && handlers.onStop(stream.destination_id), {
        title: "Stop",
        className: "stream-stop-btn",
      })
    );
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
   * @param {object} handlers - { onEditApply(destinationId, {bitrateKbps, tune,
   *   qualityLevel, rateControl}), onTransfer(destinationId, roomHandle),
   *   onFanout(captureId, roomHandle), onStop(destinationId) }
   * @param {object} [opts] - { roomOptions: [{id,name}], perStreamBitrateEditable: bool,
   *   roomsAlreadyFedByCapture: number[], editState: {tune, qualityLevel, rateControl,
   *   error}, stalledSince: number|null } — `editState` is this destination's local
   *   edit buffer for the three testing-oriented encoder tuning knobs (kept by the
   *   caller, since `DestinationRow` has no fields to read the server's current
   *   values back from — same write-only limitation the Bitrate slider already
   *   has). `stalledSince` is the caller's client-side-tracked "first observed
   *   Stalled at" timestamp (ms epoch) — see `statusBadge`'s doc comment for why
   *   this can't come from the DTO itself.
   */
  function destinationCard(dest, handlers, opts) {
    handlers = handlers || {};
    opts = opts || {};
    // Maps `capture::DestinationState`'s real variants (src/capture.rs) to
    // one of the 4 status-vocabulary families (status.css) plus its label.
    const STATE_META = {
      starting: { label: "STARTING", family: "progress" },
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
      if (dest.kind === "audio") head.appendChild(icon("volume_up", "stream-audio-glyph"));
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

    // Preview thumbnail — a static icon/gradient placeholder, not a live
    // decode. A real self-preview (sharer decoding their own outgoing stream
    // via `tile-video.js`'s `TileVideo`/`stream_video_url`, the same
    // pipeline the View tab uses) was investigated for this pass: it needs
    // (a) the sharer's own `conn_id` and (b) the capture's video `stream_id`
    // to call `toggle_watch`/`stream_video_url` against — neither is exposed
    // by any existing Tauri command (`DestinationRow` carries no `stream_id`/
    // `owner`, and no command surfaces "my own conn_id" to the frontend).
    // `src/preview.rs`'s periodic-thumbnail sender is also not reusable
    // as-is: it's the *sending* side of a receiver-facing preview stream
    // (spec §7.1, consumed by `net.rs`'s `handle_preview_stream`/the old
    // egui UI's texture upload, see `state.rs`), not something this Tauri
    // command surface exposes either. Both would need a small backend
    // addition — out of this pass's frontend-only scope — so this stays a
    // placeholder rather than blocking the rest of the card polish on it.
    const thumb = el("div", { className: "dest-thumb" }, [icon(dest.kind === "audio" ? "volume_up" : "videocam", "dest-thumb-icon")]);

    const body = el("div", { className: "dest-card-body" });

    const head = el("div", { className: "dest-card-head" });
    if (dest.kind === "audio") head.appendChild(icon("volume_up", "stream-audio-glyph"));
    head.appendChild(el("span", { className: "dest-room-label", text: dest.room_label }));
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
    const metaRow = el("div", { className: "dest-meta-row" });
    metaRow.appendChild(el("div", { className: "dest-meta", text: `${dest.width}x${dest.height} @ ${dest.framerate}fps` }));
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
    const fedRoomIds = opts.roomsAlreadyFedByCapture && opts.roomsAlreadyFedByCapture.length ? opts.roomsAlreadyFedByCapture : [dest.room_handle];
    const chipRow = el("div", { className: "dest-chip-row" });
    fedRoomIds.forEach((rid) => {
      const label = rid === dest.room_handle ? dest.room_label : (((opts.roomOptions || []).find((r) => r.id === rid) || {}).name || String(rid));
      chipRow.appendChild(el("span", { className: "dest-chip", text: label }));
    });
    const fanoutOptions = (opts.roomOptions || []).filter((r) => r.id !== dest.room_handle && !fedRoomIds.includes(r.id));
    if (fanoutOptions.length > 0) {
      const addWrap = el("span", { className: "dest-chip-inline-add" });
      const fanoutSelect = el("select", { className: "dest-room-select dest-chip-select", attrs: { hidden: "" } });
      fanoutOptions.forEach((r) => fanoutSelect.appendChild(el("option", { text: r.name, attrs: { value: r.id } })));
      const addBtn = el("button", {
        className: "dest-chip-add-btn",
        text: "+ Also send to…",
        onClick: () => {
          addBtn.hidden = true;
          fanoutSelect.hidden = false;
          fanoutSelect.focus();
        },
      });
      fanoutSelect.addEventListener("change", () => {
        handlers.onFanout && handlers.onFanout(dest.capture_id, Number(fanoutSelect.value));
        addBtn.hidden = false;
        fanoutSelect.hidden = true;
      });
      addWrap.appendChild(addBtn);
      addWrap.appendChild(fanoutSelect);
      chipRow.appendChild(addWrap);
    }
    body.appendChild(chipRow);

    // Inline bitrate edit — only meaningful in Per-Stream mode (Absolute
    // mode's allocator owns the live bitrate; showing a manual field there
    // would be a control whose input is silently overwritten, per
    // stream-control.md's "Absolute mode ... allocator writes live bitrates
    // straight into the encoders without ever touching the manual values").
    if (opts.perStreamBitrateEditable) {
      const editRow = el("div", { className: "dest-edit-row" });
      editRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Bitrate (kbps)" }));
      const slider = el("input", {
        className: "dest-bitrate-slider",
        attrs: { type: "range", min: "200", max: "40000", step: "100", value: String(dest.bitrate_kbps) },
      });
      const value = el("span", { className: "dest-bitrate-value", text: dest.bitrate_kbps + " kbps" });
      slider.addEventListener("input", () => {
        value.textContent = slider.value + " kbps";
      });
      slider.addEventListener("change", () => {
        handlers.onEditApply && handlers.onEditApply(dest.destination_id, { bitrateKbps: Number(slider.value) });
      });
      editRow.appendChild(slider);
      editRow.appendChild(value);
      body.appendChild(editRow);

      // Three testing-oriented encoder tuning knobs (`edit_destination`'s
      // `tune`/`quality_level`/`rate_control`) — same `pillSwitch` widget as
      // the Absolute/Per-Stream bandwidth-mode toggle, one row each since a
      // three-way row (Rate Control) wouldn't fit next to a two-way one
      // cleanly. Each button click applies immediately, matching the
      // Bitrate slider's auto-apply-on-change behavior above (there's no
      // separate combined "Apply" action in this panel).
      const editState = opts.editState || { tune: "low_latency", qualityLevel: "performance", rateControl: "constant" };
      const applyTuning = (patch) => handlers.onEditApply && handlers.onEditApply(dest.destination_id, patch);

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
      body.appendChild(tuneRow);

      const qualityRow = el("div", { className: "dest-edit-row" });
      qualityRow.appendChild(el("label", { className: "field-label dest-edit-label", text: "Quality Level" }));
      qualityRow.appendChild(
        pillSwitch(
          [
            { value: "performance", label: "Performance" },
            { value: "quality", label: "Quality" },
          ],
          editState.qualityLevel,
          (value) => applyTuning({ qualityLevel: value })
        )
      );
      body.appendChild(qualityRow);

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
      body.appendChild(rateControlRow);

      if (editState.error) {
        body.appendChild(el("div", { className: "dest-edit-error", text: editState.error }));
      }
    } else if (dest.computedShareKbps !== undefined) {
      body.appendChild(el("div", { className: "dest-computed-share", text: `Allocated: ${dest.computedShareKbps} kbps` }));
    }

    const actions = el("div", { className: "dest-actions" });

    // Transfer only here — fan-out ("Also send to…") moved into the chip
    // row above (`chipRow`'s dashed chip), same `handlers.onFanout` call,
    // just a restyled entry point instead of a second copy of the picker.
    const roomOptions = (opts.roomOptions || []).filter((r) => r.id !== dest.room_handle);
    if (roomOptions.length > 0) {
      const transferSelect = el("select", { className: "dest-room-select" });
      roomOptions.forEach((r) => transferSelect.appendChild(el("option", { text: r.name, attrs: { value: r.id } })));
      actions.appendChild(transferSelect);
      actions.appendChild(
        secondaryButton("Transfer", () => handlers.onTransfer && handlers.onTransfer(dest.destination_id, Number(transferSelect.value)), {
          className: "dest-action-btn",
        })
      );
    }

    actions.appendChild(dangerButton("Stop", () => handlers.onStop && handlers.onStop(dest.destination_id), { className: "dest-action-btn" }));
    body.appendChild(actions);

    wrap.appendChild(thumb);
    wrap.appendChild(body);
    return wrap;
  }

  window.Components = {
    el,
    icon,
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
    pillSwitch,
    bandwidthModeToggle,
    destinationCard,
  };
})();
