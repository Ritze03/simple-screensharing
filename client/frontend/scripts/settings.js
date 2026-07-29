// settings.js — Settings tab content: language toggle + presentation-mode
// dropdown. Per APP-SHELL.md, this tab needed the least new work — it's the
// pre-existing settings content, relocated onto the new `.card` primitive.
//
// Mock/real fallback pattern (see `superdoc/ui/FRONTEND-SHELL.md`): a real
// `invoke(...)` call on one branch, a local-state mutation on the other.
// Mock data field names/values mirror `dto::SettingsDto` — snake_case
// `presentation_mode`, and its real enum keys (`auto_vsync`/`immediate`/
// `mailbox`, from `dto::presentation_mode_key`) rather than this file's
// earlier guess (`vsync`/`reactive`/`continuous`, which don't exist on the
// backend).

(function () {
  "use strict";

  const { el, card, pillSwitch, dropdown, attachWheelStep } = window.Components;

  const MOCK_SETTINGS = {
    language: "en",
    presentation_mode: "auto_vsync",
    max_jitter_buffer_ms: 2000,
    catch_up_speed_percent: 5,
    display_name: "",
  };

  const PRESENTATION_MODES = [
    { value: "auto_vsync", label: "Auto Vsync (default)" },
    { value: "immediate", label: "Immediate" },
    { value: "mailbox", label: "Mailbox" },
  ];

  let state = Object.assign({}, MOCK_SETTINGS);

  async function fetchSettings() {
    if (window.__TAURI__) {
      const settings = await window.__TAURI__.core.invoke("get_settings");
      return {
        language: settings.language,
        presentation_mode: settings.presentation_mode,
        max_jitter_buffer_ms: settings.max_jitter_buffer_ms,
        catch_up_speed_percent: settings.catch_up_speed_percent,
        display_name: settings.display_name,
      };
    }
    return Object.assign({}, MOCK_SETTINGS);
  }

  // Both setters update local `state` on *both* branches (not just the mock
  // one) so the row reflects the new value immediately after a successful
  // invoke, rather than only after some later full refetch — see
  // `stream-control.js`'s `setBandwidthMode` for the same fix and the fuller
  // "Why" (found via the identical symptom: a toggle that visually never
  // moved after a real, successfully-applied click).
  /** The one global display name, used for every room join/create — see
   * `shell.js`'s `submitJoin`/`submitCreate` doc comments and
   * `commands::set_display_name`. Saved on blur/Enter (`change`, not
   * `input`), same "commit on change, not on every keystroke" pattern the
   * jitter-buffer slider below uses. */
  async function setDisplayName(name) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_display_name", { displayName: name });
    }
    state.display_name = name;
    render();
  }

  async function setLanguage(language) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_language", { language });
    }
    state.language = language;
    render();
  }

  async function setPresentationMode(mode) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_presentation_mode", { mode });
    }
    state.presentation_mode = mode;
    render();
  }

  async function setMaxJitterBufferMs(ms) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_max_jitter_buffer_ms", { ms });
    }
    state.max_jitter_buffer_ms = ms;
    render();
  }

  /** Ceiling on the jitter buffer's catch-up playback speed (5-10%). Same
   * mock/real fallback and local-state-mutation pattern as
   * `setMaxJitterBufferMs` right above — and, like that setting, takes
   * effect for `tile-video.js`'s pipeline via `view.js`/`viewer.js`'s own
   * one-time `get_settings()` read at startup (see those files'
   * `fetchMaxJitterBufferMs`), not by pushing live from here. */
  async function setCatchUpSpeedPercent(percent) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_catch_up_speed_percent", { percent });
    }
    state.catch_up_speed_percent = percent;
    render();
  }

  // Layout note (2026-07-28 design-fidelity pass): the prototype's Settings
  // tab is a stack of *one-concern cards*, each headed by its own micro-caps
  // label (LANGUAGE / PRESENTATION MODE / ...) — not one "SETTINGS" card with
  // every control crammed in as label-on-the-left rows. Two things were
  // genuinely missing rather than merely mis-styled, and the owner's review
  // ("I'm missing some settings") is about these:
  //
  //   1. The prose under PRESENTATION MODE explaining what the modes trade
  //      off. Without it the dropdown is three meaningless words.
  //   2. The closing info/about card. It's the only place the app states
  //      its local-only privacy property, which is a product promise, not
  //      decoration.
  //
  // No control was added or removed: `max_jitter_buffer_ms` postdates the
  // mockup and keeps its slider, moved into a PLAYBACK card of its own.
  function render() {
    const root = document.getElementById("settings-root");
    if (!root) return;
    root.innerHTML = "";

    // -- DISPLAY NAME -------------------------------------------------------
    // The one name shown to everyone in every room, set once here instead of
    // asked per join/create (2026-07-28 — see `shell.js`'s `submitJoin`).
    const nameInput = el("input", {
      className: "field-input settings-name-input",
      attrs: { type: "text", placeholder: "e.g. mo", value: state.display_name || "" },
    });
    nameInput.addEventListener("change", () => setDisplayName(nameInput.value.trim()));

    const nameBlurb = el("p", {
      className: "settings-blurb",
      text: "Shown to everyone when you join or create a room — one name for every room, not asked each time.",
    });

    // -- LANGUAGE ---------------------------------------------------------
    const langSwitch = pillSwitch(
      [
        { value: "en", label: "English" },
        { value: "de", label: "Deutsch" },
      ],
      state.language,
      (lang) => setLanguage(lang)
    );
    langSwitch.classList.add("settings-segmented");

    // -- PRESENTATION MODE ------------------------------------------------
    const select = dropdown(PRESENTATION_MODES, state.presentation_mode, (value) => setPresentationMode(value), {
      className: "settings-select",
    });

    const presentationRow = el("div", { className: "settings-control-row" }, [
      select,
      // Inline, next to the control it qualifies — per the prototype, where
      // "Restart to apply" sits beside the dropdown rather than as a
      // full-width sentence below it.
      el("span", { className: "settings-hint settings-hint-inline", text: "Restart to apply" }),
    ]);

    const presentationBlurb = el("p", {
      className: "settings-blurb",
      text:
        "Vsync is smoothest. Mailbox and Immediate trade a little tearing for lower latency — " +
        "worth trying if a stream feels behind.",
    });

    // -- PLAYBACK (jitter buffer) -----------------------------------------
    const jitterValue = el("span", { className: "settings-row-value", text: `${(state.max_jitter_buffer_ms / 1000).toFixed(1)}s` });
    const jitterSlider = el("input", {
      className: "settings-range",
      attrs: { type: "range", min: "0", max: "5000", step: "100", value: String(state.max_jitter_buffer_ms) },
    });
    jitterSlider.addEventListener("input", () => {
      jitterValue.textContent = `${(Number(jitterSlider.value) / 1000).toFixed(1)}s`;
    });
    jitterSlider.addEventListener("change", () => setMaxJitterBufferMs(Number(jitterSlider.value)));
    attachWheelStep(jitterSlider);

    const jitterRow = el("div", { className: "settings-row" }, [
      el("span", { className: "settings-row-label", text: "Max jitter buffer" }),
      jitterSlider,
      jitterValue,
    ]);

    const jitterBlurb = el("p", {
      className: "settings-blurb",
      text: "Smooths network hiccups. The buffer grows automatically only when needed, up to this ceiling.",
    });

    // -- PLAYBACK (catch-up speed) ------------------------------------------
    // Sibling control to the jitter-buffer slider right above: bounds how
    // much faster than live the buffer is allowed to play back while
    // shrinking itself back down after a jitter spike (`tile-video.js`'s
    // `updateAppliedBuffer`) — always active, this only caps how aggressive
    // it's allowed to get. Range fixed to 5-10%, per the project owner's
    // ask, so step 1 covers the whole slider.
    const catchUpValue = el("span", { className: "settings-row-value", text: `${state.catch_up_speed_percent}%` });
    const catchUpSlider = el("input", {
      className: "settings-range",
      attrs: { type: "range", min: "5", max: "10", step: "1", value: String(state.catch_up_speed_percent) },
    });
    catchUpSlider.addEventListener("input", () => {
      catchUpValue.textContent = `${Number(catchUpSlider.value)}%`;
    });
    catchUpSlider.addEventListener("change", () => setCatchUpSpeedPercent(Number(catchUpSlider.value)));
    attachWheelStep(catchUpSlider);

    const catchUpRow = el("div", { className: "settings-row" }, [
      el("span", { className: "settings-row-label", text: "Catch-up speed" }),
      catchUpSlider,
      catchUpValue,
    ]);

    const catchUpBlurb = el("p", {
      className: "settings-blurb",
      text: "How much faster than live the buffer is allowed to play back while shrinking after a jitter spike.",
    });

    // -- About ------------------------------------------------------------
    // Deliberately no version number: the prototype shows "0.4", but that
    // was the designer's placeholder and there is no command exposing the
    // real crate version to the frontend yet. Printing a made-up version is
    // worse than printing none. See FRONTEND-SHELL.md's 2026-07-28 note.
    const about = el("div", { className: "card settings-about" }, [
      el("span", { className: "material-symbols-outlined settings-about-icon", text: "info" }),
      el("span", {
        className: "settings-about-text",
        text: "Simple Screensharing — nothing but your preferences and room list is stored on this machine.",
      }),
    ]);

    const grid = el("div", { className: "settings-grid" });
    grid.appendChild(card("DISPLAY NAME", [nameInput, nameBlurb]));
    grid.appendChild(card("LANGUAGE", [langSwitch]));
    grid.appendChild(card("PRESENTATION MODE", [presentationRow, presentationBlurb]));
    grid.appendChild(card("PLAYBACK", [jitterRow, jitterBlurb, catchUpRow, catchUpBlurb]));
    grid.appendChild(about);
    root.appendChild(grid);
  }

  async function init() {
    state = await fetchSettings();
    render();
  }

  document.addEventListener("DOMContentLoaded", init);
})();
