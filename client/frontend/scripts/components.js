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
  // Room row — the sidebar ROOMS section's per-room row + expanded member
  // list, per APP-SHELL.md's three-state table.
  // -------------------------------------------------------------------

  /**
   * @param {object} room - a `SavedRoomRow` DTO shape { name, join_string, state:
   *   'joined'|'disconnected'|'reconnecting', room_handle, active_stream_count },
   *   plus a frontend-only `members` array (fetched separately via `room_detail` —
   *   no single DTO inlines both): [{conn_id, display_name, owner, stream_id,
   *   resolution, watching}].
   * @param {object} handlers - { expanded, onToggleExpand, onReconnect, onLeave, onWatch }
   */
  function renderRoomRow(room, handlers) {
    handlers = handlers || {};
    const expanded = !!handlers.expanded;
    const row = el("div", { className: "room-row room-row-" + room.state });

    const head = el("div", { className: "room-row-head" });

    if (room.state === "joined") {
      head.appendChild(
        el("button", {
          className: "room-caret",
          onClick: () => handlers.onToggleExpand && handlers.onToggleExpand(room.join_string),
          attrs: { "aria-label": expanded ? "Collapse" : "Expand" },
        }, [icon(expanded ? "expand_more" : "chevron_right")])
      );
    } else {
      head.appendChild(el("span", { className: "room-caret room-caret-spacer" }));
    }

    head.appendChild(statusDot(room.state));
    head.appendChild(el("span", { className: "room-name", text: room.name }));

    // Trailing content deliberately carries only short/fixed-width chips +
    // buttons, never the free-text state body — see the sub-line appended
    // below. `theme.rs`'s reference table puts the "not connected" body text
    // right next to the name on one line, which reads fine in the handoff's
    // proportional-font mockup; Geist Mono is a monospace face (much wider
    // per character), so the same text on one line squeezed the room name
    // down to nothing at the sidebar's width. Two lines is a deliberate,
    // documented deviation from the mock, not a missed detail.
    //
    // For "disconnected" rows specifically, the Reconnect pill + forget
    // button are *also* left off this head line (moved into the sub-meta
    // line below instead of appended to `trailing` here): the auto-generated
    // fallback room name (`Room <hex prefix>`, APP-SHELL.md's "Auto-generated
    // saved-room names") is exactly the one case where a room can't be told
    // apart from another without its full name, and at the sidebar's width a
    // name that long plus a Reconnect pill plus the forget icon left no room
    // for the name at all — it ellipsized down to an unreadable "Room
    // 01130…", losing the one distinguishing suffix. Every other row shape
    // (joined, reconnecting) keeps its trailing content on the head line as
    // before; only disconnected rows needed the extra line, since only they
    // pair a Reconnect pill with a name that's allowed to be this long.
    const trailing = el("div", { className: "room-row-trailing" });
    if (room.state === "joined") {
      trailing.appendChild(el("span", { className: "room-meta", text: room.active_stream_count + " active" }));
      trailing.appendChild(iconButton("close", () => handlers.onLeave && handlers.onLeave(room.join_string), { title: "Leave room" }));
      head.appendChild(trailing);
    } else if (room.state === "reconnecting") {
      // Per APP-SHELL.md's row-state table, "Reconnecting" has no body text
      // at all (—) — the pulsing dot alone carries the state.
      trailing.appendChild(secondaryButton("Reconnect", () => {}, { disabled: true, className: "room-reconnect-btn" }));
      head.appendChild(trailing);
    }
    row.appendChild(head);

    if (room.state === "disconnected") {
      const subMeta = el("div", { className: "room-sub-meta room-sub-meta-disconnected" });
      subMeta.appendChild(el("span", { className: "room-sub-meta-text", text: "not connected" }));
      const actions = el("div", { className: "room-row-trailing" });
      actions.appendChild(secondaryButton("Reconnect", () => handlers.onReconnect && handlers.onReconnect(room.join_string), { className: "room-reconnect-btn" }));
      actions.appendChild(iconButton("close", () => handlers.onLeave && handlers.onLeave(room.join_string), { title: "Forget room" }));
      subMeta.appendChild(actions);
      row.appendChild(subMeta);
    }

    if (room.state === "joined" && expanded) {
      const memberList = el("div", { className: "room-members" });
      (room.members || []).forEach((m) => {
        const memberRow = el("div", { className: "room-member-row" });
        memberRow.appendChild(el("span", { className: "room-member-name", text: m.display_name }));
        memberRow.appendChild(
          el("span", {
            className: "room-member-caps",
            text: m.resolution || "",
          })
        );
        memberRow.appendChild(
          iconButton(m.watching ? "visibility" : "play_arrow", () => handlers.onWatch && handlers.onWatch(room.join_string, m), {
            title: m.watching ? "Watching" : "Watch",
            className: m.watching ? "watch-active" : "",
          })
        );
        memberList.appendChild(memberRow);
      });
      if (!room.members || room.members.length === 0) {
        memberList.appendChild(el("div", { className: "room-members-empty", text: "No other members yet." }));
      }
      row.appendChild(memberList);
    }

    return row;
  }

  // -------------------------------------------------------------------
  // Stream row — the sidebar STREAMS section's per-destination row.
  // -------------------------------------------------------------------

  /**
   * @param {object} stream - a `DestinationRow` DTO shape { kind: 'video'|'audio',
   *   width, height, framerate }, plus a frontend-only `room_label` (looked up from
   *   the room's `SavedRoomRow.name` by `room_handle` — `DestinationRow` itself only
   *   carries the numeric handle, not a display name).
   */
  function renderStreamRow(stream) {
    const row = el("div", { className: "stream-row" });
    if (stream.kind === "audio") row.appendChild(icon("volume_up", "stream-audio-glyph"));
    row.appendChild(
      el("span", {
        className: "stream-label",
        text: `${stream.room_label}: ${stream.width}x${stream.height}@${stream.framerate}`,
      })
    );
    return row;
  }

  /** Empty-state block: message + optional primary action. */
  function renderEmptyState(message, actionLabel, onAction) {
    const wrap = el("div", { className: "empty-state" });
    wrap.appendChild(el("p", { className: "empty-state-message", text: message }));
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
   * @param {object} handlers - { onEditApply(destinationId, {bitrateKbps}), onTransfer(destinationId, roomHandle),
   *   onFanout(captureId, roomHandle), onStop(destinationId) }
   * @param {object} [opts] - { roomOptions: [{id,name}], perStreamBitrateEditable: bool,
   *   roomsAlreadyFedByCapture: number[] }
   */
  function destinationCard(dest, handlers, opts) {
    handlers = handlers || {};
    opts = opts || {};
    const STATE_LABEL = {
      starting: "Starting",
      live: "Live",
      rebuilding: "Applying…",
      stalled: "Waiting for capture…",
      failed: "Failed",
    };

    const wrap = el("div", { className: "dest-card dest-card-" + dest.state });

    const head = el("div", { className: "dest-card-head" });
    head.appendChild(statusDot(dest.state));
    if (dest.kind === "audio") head.appendChild(icon("volume_up", "stream-audio-glyph"));
    head.appendChild(el("span", { className: "dest-room-label", text: dest.room_label }));
    head.appendChild(
      el("span", {
        className: "dest-state-badge dest-state-badge-" + dest.state,
        text: STATE_LABEL[dest.state] || dest.state,
      })
    );
    wrap.appendChild(head);

    if (dest.pendingRoleText) {
      wrap.appendChild(el("div", { className: "dest-pending-role", text: dest.pendingRoleText }));
    }

    wrap.appendChild(el("div", { className: "dest-meta", text: `${dest.width}x${dest.height} @ ${dest.framerate}fps` }));

    // Live captured/sent FPS readout — populated out-of-band by
    // `stream-control.js`'s `updateStats()` on every `stream-stats-updated`
    // tick (every ~250ms), via direct `textContent` writes rather than going
    // through this card's normal sig-diffed render path, so a stats tick
    // never triggers a full card rebuild. Starts hidden until the first tick
    // arrives for this destination.
    wrap.appendChild(el("div", { className: "dest-fps-readout", attrs: { hidden: "" } }));

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
      wrap.appendChild(editRow);
    } else if (dest.computedShareKbps !== undefined) {
      wrap.appendChild(el("div", { className: "dest-computed-share", text: `Allocated: ${dest.computedShareKbps} kbps` }));
    }

    const actions = el("div", { className: "dest-actions" });

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

      const fanoutOptions = roomOptions.filter((r) => !(opts.roomsAlreadyFedByCapture || []).includes(r.id));
      if (fanoutOptions.length > 0) {
        const fanoutSelect = el("select", { className: "dest-room-select" });
        fanoutOptions.forEach((r) => fanoutSelect.appendChild(el("option", { text: r.name, attrs: { value: r.id } })));
        actions.appendChild(fanoutSelect);
        actions.appendChild(
          secondaryButton("Also send to…", () => handlers.onFanout && handlers.onFanout(dest.capture_id, Number(fanoutSelect.value)), {
            className: "dest-action-btn",
          })
        );
      }
    }

    actions.appendChild(dangerButton("Stop", () => handlers.onStop && handlers.onStop(dest.destination_id), { className: "dest-action-btn" }));
    wrap.appendChild(actions);

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
