// settings.js — Settings tab content: language toggle + presentation-mode
// dropdown. Per APP-SHELL.md, this tab needed the least new work — it's the
// pre-existing settings content, relocated onto the new `.card` primitive;
// no settings added, removed, or redesigned.
//
// Mock-data-first, same `if (window.__TAURI__) {...} else {...}` pattern as
// the other two tabs' scripts: a real `invoke(...)` stub (not implemented —
// a later worker fills it in) on one branch, a local-state mutation on the
// other, so this renders and is interactive in a plain browser.

(function () {
  "use strict";

  const { el, card, pillSwitch } = window.Components;

  const MOCK_SETTINGS = { language: "en", presentationMode: "vsync" };

  const PRESENTATION_MODES = [
    { value: "vsync", label: "Vsync (default)" },
    { value: "reactive", label: "Reactive" },
    { value: "continuous", label: "Continuous" },
  ];

  let state = Object.assign({}, MOCK_SETTINGS);

  async function fetchSettings() {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("get_settings"); // TODO: not implemented yet — backend worker's scope.
    }
    return Object.assign({}, MOCK_SETTINGS);
  }

  async function setLanguage(lang) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_language", { lang }); // TODO: not implemented yet.
    } else {
      state.language = lang;
    }
    render();
  }

  async function setPresentationMode(mode) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_presentation_mode", { mode }); // TODO: not implemented yet.
    } else {
      state.presentationMode = mode;
    }
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
    select.value = state.presentationMode;
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
