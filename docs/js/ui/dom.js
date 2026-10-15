// DOM helpers and the reusable row patterns of the room-centric layout (same names as the mockup and as the egui
// widgets: ctrlRow, roomRow, sharerRow, personRow, destRow, popItem, btnRow, card, gridRows, modalShell).
// Each pattern is the ONLY way its row type is built, so rows of a type cannot drift apart.

export function h(tag, attrs, ...kids) {
  const n = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v == null || v === false) continue;
      if (k === "class") n.className = v;
      else if (k === "text") n.textContent = v;
      else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
      else if (k === "style") n.setAttribute("style", v);
      else if (k === "value" && (tag === "input" || tag === "select")) n.value = v;
      else n.setAttribute(k, v === true ? "" : v);
    }
  }
  for (const c of kids.flat(Infinity)) if (c != null && c !== false) n.appendChild(typeof c === "string" || typeof c === "number" ? document.createTextNode(String(c)) : c);
  return n;
}
export const ic = (name, cls) => h("span", { class: "ms" + (cls ? " " + cls : ""), "aria-hidden": "true", text: name });
export const T = (text, cls) => h("span", { class: "txt" + (cls ? " " + cls : ""), text });

/* ---- layout primitives (egui 1:1) ---- opts: { g: gap step 0–4 (default 2 = 8px), cls, tag, attrs, style } */
function box(kind, o, kids) {
  o = o || {};
  const attrs = Object.assign({}, o.attrs || {});
  attrs.class = kind + " g" + (o.g == null ? 2 : o.g) + (o.cls ? " " + o.cls : "");
  if (o.style) attrs.style = o.style;
  return h(o.tag || "div", attrs, kids);
}
export const V = (o, ...kids) => box("vb", o, kids);
export const H = (o, ...kids) => box("hb", o, kids);
// `o.min`: `w` is a minimum and the slot grows to its content (a pill slot:
// a fixed 84 overflowed "ÜBERNIMMT"/"STARTING"; the native bar clipped
// "CONNECTING", user report 2026-10-15).
export const SL = (w, kids, o) => {
  o = o || {};
  const size = o.min ? `min-width:${w}px;flex-basis:auto` : `width:${w}px;flex-basis:${w}px`;
  return h(o.tag || "div", Object.assign({}, o.attrs || {}, { class: "sl" + (o.al ? " " + o.al : "") + (o.cls ? " " + o.cls : ""), style: size + (o.style ? ";" + o.style : "") }), kids);
};
export const FILL = (kids, o) => {
  o = o || {};
  return h("div", Object.assign({}, o.attrs || {}, { class: "fl" + (o.v ? " v" : "") + (o.al ? " " + o.al : "") + (o.cls ? " " + o.cls : ""), style: o.style || null }), kids);
};

export const pct = (inp) => inp.style.setProperty("--pct", ((inp.value - inp.min) / (inp.max - inp.min)) * 100 + "%");
export const fmtKbps = (k) => (k >= 1000 ? (k / 1000).toFixed(1) + " Mbps" : Math.round(k) + " kbps");

export const LABEL_W = 120;
export const VALUE_W = 72;

/** Control row: hbox [slot 120 label][fill control][slot 72 value?] */
export function ctrlRow(label, control, value, tip, forId) {
  const lab = label == null ? null : h(forId ? "label" : "span", { class: "lbl", for: forId || null }, label, tip ? h("span", { class: "tip ms", title: tip, text: "info" }) : null);
  return H({ cls: "crow" }, SL(LABEL_W, lab, { al: "l" }), FILL(control), value === undefined ? null : SL(VALUE_W, value, { al: "r" }));
}
export const valBox = (text, cls) => h("span", { class: "val" + (cls ? " " + cls : ""), text });

/** Slider + value box. `put` runs on every input; `onCommit` once on release. */
export function sliderCtl(id, label, min, max, step, get, put, fmt, onCommit) {
  const val = valBox(fmt(get()));
  const sl = h("input", { type: "range", min: String(min), max: String(max), step: String(step), value: String(get()), id, "aria-label": label });
  sl.addEventListener("input", () => {
    put(Number(sl.value));
    val.textContent = fmt(get());
    pct(sl);
  });
  sl.addEventListener("change", () => onCommit && onCommit());
  return [sl, val];
}
export function sliderRow(label, id, min, max, step, get, put, fmt, tip, onCommit) {
  const [sl, val] = sliderCtl(id, label, min, max, step, get, put, fmt, onCommit);
  return ctrlRow(label, sl, val, tip, id);
}

/** Segmented control. opts: [[value, label, disabled?, title?]] */
export function seg(opts, cur, onPick) {
  return h("div", { class: "seg", role: "radiogroup" },
    opts.map(([v, l, disabled, title]) => h("button", {
      type: "button", role: "radio", "aria-checked": String(v === cur), title: title || l, class: v === cur ? "on" : "", disabled: !!disabled, onclick: () => onPick(v),
    }, l)));
}

/** Checkbox styled like the native one. */
export function cbx(label, on, onToggle, { disabled = false, title = null, style = null, id = null } = {}) {
  return h("span", {
    class: "cbx" + (on ? " on" : "") + (disabled ? " disabled" : ""), role: "checkbox", "aria-checked": String(!!on), "aria-disabled": disabled ? "true" : null,
    tabindex: disabled ? null : "0", title, style, id,
    onclick: () => !disabled && onToggle(),
    onkeydown: (e) => { if (!disabled && (e.key === " " || e.key === "Enter")) { e.preventDefault(); onToggle(); } },
  }, h("span", { class: "box" }, ic("check")), label);
}

/** Card: Frame -> vbox [hbox title][...rows] */
export function card(title, count, ...rows) {
  return V({ cls: "gcard", g: 2 },
    title == null ? null : H({ g: 2 }, h("h4", { class: "section-label" }, title, count != null ? h("span", { class: "count", text: String(count) }) : null)),
    rows);
}
/** Section heading in a scroll area: hbox [label][count][rule?] */
export function secHead(title, count, rule) {
  return H({ g: 2 }, h("h3", { class: "section-label" }, title, count != null ? h("span", { class: "count", text: String(count) }) : null), rule ? h("span", { class: "rule" }) : null);
}
/** Sidebar room row: hbox [slot 18 lead][fill name][slot 18 me-live][slot 28 number][slot 22 unit] */
export function roomRow({ lead, name, me, num, unit, sel, onclick, title, numAttrs }) {
  return H({ tag: "button", cls: "p-row" + (sel ? " sel" : ""), attrs: { type: "button", onclick, title: title || null } },
    SL(18, lead), FILL(T(name, "name")), me === false ? null : SL(18, me || null),
    SL(28, h("span", Object.assign({ class: "num", text: num == null ? "" : String(num) }, numAttrs || {})), { al: "r" }), SL(22, unit || null, { al: "l" }));
}
/** Sidebar sharer line: hbox [slot 18 indent][slot 18 icon][fill name][slot 28 number][slot 22 unit] */
export function sharerRow({ icon, name, num, unit, onclick, title, numAttrs }) {
  return H({ cls: "p-sub", attrs: { onclick, title: title || null, role: "button", tabindex: "0", onkeydown: (e) => e.key === "Enter" && onclick && onclick() } },
    SL(18, null), SL(18, icon), FILL(T(name)),
    SL(28, h("span", Object.assign({ class: "num", text: num == null ? "" : String(num) }, numAttrs || {})), { al: "r" }),
    SL(22, h("span", { class: "unit", text: unit || "" }), { al: "l" }));
}
/** People row: hbox [slot 22 avatar][fill name][slot 44 pill][slot 18 eye] */
export function personRow({ avatar, name, you, youLabel, pill, eye, idle, stale }) {
  return H({ cls: "member" + (idle ? " idle" : "") + (stale ? " stale" : "") },
    SL(22, avatar), FILL(h("span", { class: "txt name" }, name, you ? h("span", { class: "you", text: youLabel }) : null)), SL(44, pill, { al: "r" }), SL(18, eye));
}
/** Destination row: hbox [slot 18 icon][fill name][slot min 84 pill][slot 76 kbps][slot 32 action] */
export function destRow({ cur, name, curLabel, pill, kbps, action }) {
  return H({ cls: "destrow" + (cur ? " cur" : "") },
    SL(18, ic(cur ? "radio_button_checked" : "meeting_room", "ico")),
    FILL(h("span", { class: "txt name" }, name, cur ? h("small", { text: curLabel }) : null)),
    SL(84, pill, { al: "l", min: true }), SL(76, h("span", { class: "kbps", text: kbps }), { al: "r" }), SL(32, action));
}
/** Popup item: hbox [slot 18 icon][fill vbox(label, sub)][slot 23 trailing?] */
export function popItem({ icon, iconEl, label, sub, onclick, cls, disabled, trailing, role }) {
  return H({ tag: "button", cls: "pi" + (sub ? " two" : "") + (cls ? " " + cls : ""), attrs: { type: "button", role: role || "menuitem", disabled: !!disabled, onclick } },
    SL(18, iconEl || ic(icon, "ico")),
    FILL([T(label), sub ? h("small", { class: "txt", text: sub }) : null], { v: true }),
    trailing ? SL(23, trailing) : null);
}
/** Equal-width button row: hbox [fill btn][fill btn]... */
export const btnRow = (...btns) => H({ g: 2 }, btns.filter(Boolean).map((b) => { b.classList.add("wide"); return FILL(b); }));
/** Grid of equal columns built from nested boxes: vbox [hbox row [fill col]...]... */
export function gridRows(items, cols, renderItem, maxW) {
  const rows = [];
  for (let i = 0; i < items.length; i += cols) {
    const chunk = items.slice(i, i + cols);
    const cells = chunk.map((it) => FILL(renderItem(it), { v: true }));
    while (cells.length < cols) cells.push(FILL(null));
    rows.push(H({ g: 3, cls: "st", style: maxW ? `max-width:${maxW}` : null }, cells));
  }
  return V({ g: 3 }, rows);
}
/** Card/tile column count from the central width (egui: available_width / 272). */
export const cols = (w) => Math.max(1, Math.min(4, Math.floor((w + 12) / 272)));

export function emptyState(icon, title, text, actions, { err = false, errText = null } = {}) {
  return V({ cls: "empty", g: 2 }, ic(icon, "big" + (err ? " err" : "")), T(title, "b"), text ? h("span", { text }) : null,
    errText ? h("span", { class: "errtext", text: errText }) : null,
    actions ? H({ g: 0, style: "width:320px;margin-top:var(--s2)" }, FILL(actions)) : null);
}
export function banner(icon, text, action, { error = false } = {}) {
  return H({ cls: "banner" + (error ? " error" : ""), g: 2 }, SL(18, ic(icon)), FILL(T(text)), action || null);
}
export function meter(key) {
  return h("span", { class: "meter", "data-meter": key || "", "aria-hidden": "true" }, h("i"), h("i"), h("i"), h("i"), h("i"));
}
/** Modal shell: vbox [hbox head band][scroll vbox body][hbox foot] */
export function modalShell(cls, title, sub, close, body, foot, extraHead, closeLabel) {
  return V({ cls: "modal " + (cls || ""), g: 0, attrs: { role: "dialog", "aria-modal": "true", "aria-label": title } },
    H({ cls: "band modal-head", g: 3 },
      FILL(V({ g: 0 }, h("h3", { class: "txt", text: title }), sub ? T(sub, "sub") : null), { v: true }),
      extraHead || null,
      SL(32, h("button", { class: "icon-btn", type: "button", "aria-label": closeLabel || "Close", title: closeLabel || "Close", onclick: close }, ic("close")))),
    V({ cls: "modal-body", g: 3 }, body),
    foot ? H({ cls: "modal-foot", g: 2 }, foot) : null);
}
/**
 * Avatar colour, a pure function of the display name (cross-client contract: the native client implements the same,
 * `client/src/theme.rs` `avatar_color`, and both test suites assert the same vectors).
 * hue = FNV-1a 32-bit over the UTF-8 bytes of trim().toLowerCase(), mod 360; colour = HSL(hue, 60 %, 34 %).
 * L = 34 % keeps white initials >= 3:1 on every hue (worst case yellow, 3.6:1).
 */
export function avatarHue(name) {
  let x = 2166136261;
  for (const b of new TextEncoder().encode(String(name == null ? "" : name).trim().toLowerCase())) x = Math.imul(x ^ b, 16777619) >>> 0;
  return x % 360;
}
const AVATAR_S = 0.6, AVATAR_L = 0.34;
export function avatarColor(name) {
  const hue = avatarHue(name);
  const a = AVATAR_S * Math.min(AVATAR_L, 1 - AVATAR_L);
  const f = (n) => {
    const k = (n + hue / 30) % 12;
    const v = Math.round(255 * (AVATAR_L - a * Math.max(-1, Math.min(k - 3, 9 - k, 1))));
    return v.toString(16).padStart(2, "0");
  };
  return "#" + f(0) + f(8) + f(4);
}
/** Inline style for an `.avatar` element showing `name`'s colour (computed to the exact 8-bit hex, not left to CSS hsl()). */
export const avatarBg = (name) => "background:" + avatarColor(name);
export const initial = (name) => (String(name || "?").trim()[0] || "?").toUpperCase();
