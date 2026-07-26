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
   * @param {object} room - { id, name, state: 'joined'|'disconnected'|'reconnecting', activeCount, members: [{name, resolution, fps, watching}] }
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
          onClick: () => handlers.onToggleExpand && handlers.onToggleExpand(room.id),
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
    const trailing = el("div", { className: "room-row-trailing" });
    if (room.state === "joined") {
      trailing.appendChild(el("span", { className: "room-meta", text: room.activeCount + " active" }));
      trailing.appendChild(iconButton("close", () => handlers.onLeave && handlers.onLeave(room.id), { title: "Leave room" }));
    } else if (room.state === "reconnecting") {
      // Per APP-SHELL.md's row-state table, "Reconnecting" has no body text
      // at all (—) — the pulsing dot alone carries the state.
      trailing.appendChild(secondaryButton("Reconnect", () => {}, { disabled: true, className: "room-reconnect-btn" }));
    } else {
      trailing.appendChild(secondaryButton("Reconnect", () => handlers.onReconnect && handlers.onReconnect(room.id), { className: "room-reconnect-btn" }));
      trailing.appendChild(iconButton("close", () => handlers.onLeave && handlers.onLeave(room.id), { title: "Forget room" }));
    }
    head.appendChild(trailing);
    row.appendChild(head);

    if (room.state === "disconnected") {
      row.appendChild(el("div", { className: "room-sub-meta", text: "not connected" }));
    }

    if (room.state === "joined" && expanded) {
      const memberList = el("div", { className: "room-members" });
      (room.members || []).forEach((m) => {
        const memberRow = el("div", { className: "room-member-row" });
        memberRow.appendChild(el("span", { className: "room-member-name", text: m.name }));
        memberRow.appendChild(
          el("span", {
            className: "room-member-caps",
            text: m.resolution ? `${m.resolution}@${m.fps || "—"}` : "",
          })
        );
        memberRow.appendChild(
          iconButton(m.watching ? "visibility" : "play_arrow", () => handlers.onWatch && handlers.onWatch(room.id, m.name), {
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

  /** @param {object} stream - { roomLabel, resolution, fps, isAudio } */
  function renderStreamRow(stream) {
    const row = el("div", { className: "stream-row" });
    if (stream.isAudio) row.appendChild(icon("volume_up", "stream-audio-glyph"));
    row.appendChild(
      el("span", {
        className: "stream-label",
        text: `${stream.roomLabel}: ${stream.resolution}@${stream.fps}`,
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
  };
})();
