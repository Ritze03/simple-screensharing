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

  const { el, card, pillSwitch } = window.Components;

  const MOCK_SETTINGS = { language: "en", presentation_mode: "auto_vsync" };

  const PRESENTATION_MODES = [
    { value: "auto_vsync", label: "Auto Vsync (default)" },
    { value: "immediate", label: "Immediate" },
    { value: "mailbox", label: "Mailbox" },
  ];

  let state = Object.assign({}, MOCK_SETTINGS);

  async function fetchSettings() {
    if (window.__TAURI__) {
      const settings = await window.__TAURI__.core.invoke("get_settings");
      return { language: settings.language, presentation_mode: settings.presentation_mode };
    }
    return Object.assign({}, MOCK_SETTINGS);
  }

  // Both setters update local `state` on *both* branches (not just the mock
  // one) so the row reflects the new value immediately after a successful
  // invoke, rather than only after some later full refetch — see
  // `stream-control.js`'s `setBandwidthMode` for the same fix and the fuller
  // "Why" (found via the identical symptom: a toggle that visually never
  // moved after a real, successfully-applied click).
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

  function render() {
    const root = document.getElementById("settings-root");
    if (!root) return;
    root.innerHTML = "";

    const langRow = el("div", { className: "settings-row" });
    langRow.appendChild(el("span", { className: "settings-row-label", text: "Language" }));
    langRow.appendChild(
      pillSwitch(
        [
          { value: "en", label: "English" },
          { value: "de", label: "Deutsch" },
        ],
        state.language,
        (lang) => setLanguage(lang)
      )
    );

    const presentationRow = el("div", { className: "settings-row" });
    presentationRow.appendChild(el("span", { className: "settings-row-label", text: "Presentation mode" }));
    const select = el("select", { className: "settings-select" });
    PRESENTATION_MODES.forEach((m) => select.appendChild(el("option", { text: m.label, attrs: { value: m.value } })));
    select.value = state.presentation_mode;
    select.addEventListener("change", () => setPresentationMode(select.value));
    presentationRow.appendChild(select);

    const hint = el("div", { className: "settings-hint", text: "Presentation mode changes take effect after restarting the app." });

    const grid = el("div", { className: "settings-grid" });
    grid.appendChild(card("SETTINGS", [langRow, presentationRow, hint]));
    root.appendChild(grid);
  }

  async function init() {
    state = await fetchSettings();
    render();
  }

  document.addEventListener("DOMContentLoaded", init);
})();
