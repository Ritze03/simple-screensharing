// DOM helpers and the reusable row patterns of the room-centric layout (same names as the mockup and as the egui
// widgets: ctrlRow, roomRow, sharerRow, destRow, btnRow, card, modalShell).
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
// The tile volume slider is 0-500 % in steps of 5 (core/audio.js MAX_VOLUME); 100 % is the default and gets a detent:
// anything within VOLUME_SNAP_PCT of it lands exactly on it (about 5 %/px at the 110 px slider).
export const VOLUME_MAX_PCT = 500;
export const VOLUME_SNAP_PCT = 10;
export const snapVolumePct = (v) => (Math.abs(v - 100) <= VOLUME_SNAP_PCT ? 100 : v);
/** One mouse-wheel step of the tile volume (task #113): 5 % up to 200 %, 10 % above it, landing on a multiple of the
 *  step and clamped to 0-500 %. The step is the one of the segment being entered: 195 -> 200 -> 210 up, 210 -> 200 -> 195
 *  down. Mirrors the native `ui::wheel_step`; no detent (95 -> 100 -> 105). */
export const wheelStepVolumePct = (v, up) => {
  if (up) {
    const s = v >= 200 ? 10 : 5;
    return Math.min(VOLUME_MAX_PCT, (Math.floor(v / s) + 1) * s);
  }
  const s = v > 200 ? 10 : 5;
  return Math.floor(Math.max(v - 1, 0) / s) * s;
};
/** Make the mouse wheel step a volume range input and fire its `input` event (so the normal handler runs). The wheel is
 *  consumed (preventDefault, needs a non-passive listener) so the page under the slider does not scroll. One notch
 *  (|deltaY| ~100, or deltaMode lines) is one step; smaller touchpad deltas accumulate to half a notch first. */
export function volumeWheel(slider) {
  let acc = 0;
  slider.addEventListener("wheel", (e) => {
    e.preventDefault();
    acc += e.deltaMode === 0 ? e.deltaY : e.deltaY * 33;
    if (Math.abs(acc) < 50) return;
    let n = Math.max(1, Math.round(Math.abs(acc) / 100));
    const up = acc < 0; // wheel up / away from the user = negative deltaY = louder
    acc = 0;
    while (n-- > 0) slider.value = String(wheelStepVolumePct(Number(slider.value), up));
    const ev = new Event("input", { bubbles: true });
    ev.fromWheel = true; // the handler skips the 100 % detent for these: 100 -> 105 must not snap back to 100
    slider.dispatchEvent(ev);
  }, { passive: false });
  return slider;
}
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
/** Destination row: hbox [slot 18 icon][fill name][slot min 84 pill][slot 76 kbps][slot 32 action] */
export function destRow({ cur, name, curLabel, pill, kbps, action }) {
  return H({ cls: "destrow" + (cur ? " cur" : "") },
    SL(18, ic(cur ? "radio_button_checked" : "meeting_room", "ico")),
    FILL(h("span", { class: "txt name" }, name, cur ? h("small", { text: curLabel }) : null)),
    SL(84, pill, { al: "l", min: true }), SL(76, h("span", { class: "kbps", text: kbps }), { al: "r" }), SL(32, action));
}
/** Equal-width button row: hbox [fill btn][fill btn]... */
export const btnRow = (...btns) => H({ g: 2 }, btns.filter(Boolean).map((b) => { b.classList.add("wide"); return FILL(b); }));
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
 * Avatar gradient, a pure function of the display name (cross-client contract: the native client implements the same,
 * `client/src/theme.rs` `avatar_colors`, and both test suites assert the same vectors).
 * hue = FNV-1a 32-bit over the UTF-8 bytes of trim().toLowerCase(), mod 360;
 * stop A (top-left) = HSL(hue, 60 %, 36 %), stop B (bottom-right) = HSL(hue - 10, 60 %, 24 %), at 140deg.
 * L_A = 36 % keeps white initials >= 3:1 against the lighter stop on every hue (worst case yellow, 3.26:1).
 */
export function avatarHue(name) {
  let x = 2166136261;
  for (const b of new TextEncoder().encode(String(name == null ? "" : name).trim().toLowerCase())) x = Math.imul(x ^ b, 16777619) >>> 0;
  return x % 360;
}
/** CSS hsl() to an exact 8-bit "#rrggbb", each channel Math.round(255 * v) (same as theme.rs `hsl_rgb`). */
function hslHex(hue, s, l) {
  const a = s * Math.min(l, 1 - l);
  const f = (n) => {
    const k = (n + hue / 30) % 12;
    const v = Math.round(255 * (l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1))));
    return v.toString(16).padStart(2, "0");
  };
  return "#" + f(0) + f(8) + f(4);
}
const AVATAR_S = 0.6, AVATAR_LA = 0.36, AVATAR_LB = 0.24, AVATAR_SHIFT_B = 350;
/** `[stopA, stopB]` hex colours of `name`'s avatar gradient (A lighter, top-left). `hue` overrides the name's hue (an emoji's). */
export function avatarColors(name, hue = avatarHue(name)) {
  return [hslHex(hue, AVATAR_S, AVATAR_LA), hslHex((hue + AVATAR_SHIFT_B) % 360, AVATAR_S, AVATAR_LB)];
}
/** The picker's emoji, 8 per row in three groups (rows 0 | 1-5 | 6; menus.js sets the groups apart): the user's own list (2026-11-05), same as native `widgets/emoji.rs` `EMOJI`. An explicit array: spreading a string would split 🐻‍❄️, 🐿️ and 🕊️. Any other single emoji can still be pasted; native draws any one grapheme of at most 16 bytes that shapes to a single bitmap glyph in its embedded Noto font. */
export const EMOJI = [
  "🤪", "🤤", "🤠", "🫪", "🤓", "🤔", "🥶", "😡",
  "🐒", "🦁", "🐷", "🐑", "🐹", "🦥", "🐼", "🐻‍❄️",
  "🐻", "🐿️", "🦫", "🦔", "🦒", "🐮", "🐱", "🫏",
  "🐔", "🐓", "🐥", "🐣", "🐦", "🐧", "🕊️", "🦜",
  "🐸", "🐢", "🦎", "🐍", "🐬", "🫍", "🦭", "🐙",
  "🦀", "🐌", "🐛", "🐜", "🐝", "🪰", "🪱", "🦤",
  "🥒", "🍟", "🍕", "🌭", "🥙", "🍿", "🍔", "🍝",
];
const EMOJI_FONT = '"Noto Color Emoji", "Apple Color Emoji", "Segoe UI Emoji", "Twemoji Mozilla", sans-serif';
/** Greyish background of a colourless emoji (like 💀): the replica's `hsl(222, .14, .36 / .24)` slate, not a hue at all. */
const SLATE = [hslHex(222, 0.14, 0.36), hslHex(222, 0.14, 0.24)];
const emojiCache = new Map();
/**
 * The point an emoji balances on, in pixel coordinates of `alpha` (`w` x `h`, one alpha byte per pixel, solid = >= 128): the
 * midpoint of the solid pixels' mass centre and the centre of their smallest enclosing circle. Port of native
 * `widgets/emoji.rs` `balance_point` (a box centre does not balance asymmetric shapes; mass alone overshoots the moon and the
 * guitar, the circle alone lifts the gamepad). `null` when nothing is solid. Pure: no canvas.
 */
export function balancePoint(alpha, w, h) {
  let sx = 0, sy = 0, n = 0;
  const edge = [];
  for (let y = 0; y < h; y++) {
    const cy = y + 0.5;
    let l = -1, r = -1;
    for (let x = 0; x < w; x++) {
      if (alpha[y * w + x] < 128) continue;
      if (l < 0) l = x;
      r = x;
      sx += x + 0.5; sy += cy; n++;
    }
    if (l >= 0) edge.push([l, cy], [r + 1, cy]);
  }
  if (!n) return null;
  const mass = [sx / n, sy / n];
  // Badoiu-Clarkson: step towards the farthest point by a shrinking fraction
  let c = mass;
  for (let i = 1; i <= 400; i++) {
    let far = edge[0], best = -1;
    for (const p of edge) {
      const d = (p[0] - c[0]) ** 2 + (p[1] - c[1]) ** 2;
      if (d > best) { best = d; far = p; }
    }
    const k = 1 / (i + 1);
    c = [c[0] + (far[0] - c[0]) * k, c[1] + (far[1] - c[1]) * k];
  }
  return [(mass[0] + c[0]) / 2, (mass[1] + c[1]) / 2];
}
/**
 * What an emoji looks like, measured once by drawing it on a 48 px canvas (a browser has system emoji fonts, so this is the
 * replica's approach; native does the same on its embedded Noto bitmaps): `hue` = its strongest hue in 10-degree bins (so a
 * mix of colours cannot average to mud), `null` when it has almost no colour; `dx`/`dy` = how far its balance point
 * (`balancePoint`, the same rule as native) is from the centre of its em box, in em, so the avatar can centre the emoji
 * (D1: "crop and centre", native `paint_avatar_emoji`). Without a canvas (a test page without a DOM) or on a failure: no hue, no shift.
 */
export function emojiInfo(e) {
  if (emojiCache.has(e)) return emojiCache.get(e);
  let info = { hue: null, dx: 0, dy: 0 };
  try {
    const c = document.createElement("canvas");
    c.width = c.height = 48;
    const x = c.getContext("2d", { willReadFrequently: true });
    x.font = `32px ${EMOJI_FONT}`;
    x.textAlign = "center";
    x.textBaseline = "middle";
    x.fillText(e, 24, 24);
    const d = x.getImageData(0, 0, 48, 48).data;
    const bins = new Array(36).fill(0);
    let coloured = 0;
    const alpha = new Uint8Array(48 * 48);
    for (let i = 0; i < d.length; i += 4) {
      alpha[i / 4] = d[i + 3];
      if (d[i + 3] < 128) continue;
      const r = d[i], g = d[i + 1], b = d[i + 2], hi = Math.max(r, g, b), lo = Math.min(r, g, b), sat = hi ? (hi - lo) / hi : 0;
      if (sat < 0.25 || hi < 40) continue;
      const dl = hi - lo, hue = (60 * (hi === r ? (g - b) / dl + 6 : hi === g ? (b - r) / dl + 2 : (r - g) / dl + 4)) % 360;
      bins[Math.floor(hue / 10)] += sat;
      coloured++;
    }
    info = { hue: coloured < 40 ? null : bins.indexOf(Math.max(...bins)) * 10 + 5, dx: 0, dy: 0 };
    const bp = balancePoint(alpha, 48, 48);
    if (bp) info.dx = (bp[0] - 24) / 32, info.dy = (bp[1] - 24) / 32;
  } catch {}
  emojiCache.set(e, info);
  return info;
}
/** Inline style for an `.avatar` element showing `name`'s gradient (exact 8-bit hex stops, not left to CSS hsl()); with an
 *  `emoji` the background comes from the emoji instead (a colourless one gets a neutral slate). */
export const avatarBg = (name, emoji = "") => {
  let [a, b] = [null, null];
  if (emoji) {
    const { hue } = emojiInfo(emoji);
    [a, b] = hue === null ? SLATE : avatarColors(name, hue);
  } else [a, b] = avatarColors(name);
  return `background:linear-gradient(140deg, ${a}, ${b})`;
};
export const initial = (name) => (String(name || "?").trim()[0] || "?").toUpperCase();
/** The one avatar builder: a round gradient with the person's emoji (a thin black outline that follows its shape, centred by
 *  its visible pixels) or else their initial. `opts`: `sm` (22 px; the default 32, the strip sets its own size in CSS), `pres`
 *  (presence dot), `cls`, `kids` (badges). */
export function avatarEl(name, emoji, { sm = false, pres = null, cls = "", kids = [] } = {}) {
  const e = emoji || "";
  let face;
  if (e) {
    const { dx, dy } = emojiInfo(e);
    face = h("span", { class: "emo", style: `translate:${(-dx).toFixed(3)}em ${(-dy).toFixed(3)}em`, text: e });
  } else face = initial(name);
  return h("span", { class: "avatar" + (sm ? " sm" : "") + (cls ? " " + cls : ""), style: avatarBg(name, e) }, face, pres == null ? null : h("span", { class: "pres" + (pres ? "" : " off") }), kids);
}
/** The last emoji of a text as one grapheme (a flag or a ZWJ family stays whole); "" when there is none. Pasting over or after the old one both work. */
const isEmoji = (g) => /\p{Extended_Pictographic}|\p{Regional_Indicator}|⃣/u.test(g);
export const pickEmoji = (text) => [...new Intl.Segmenter(undefined, { granularity: "grapheme" }).segment(String(text || ""))].map((x) => x.segment).filter(isEmoji).at(-1) || "";
