#!/usr/bin/env python3
"""Check that every i18n key used in client/src/ exists in en.yml and de.yml,
and that the %{placeholder} sets match between the two locales.

Run:  python3 -I client/locales/check_keys.py   (from anywhere; no deps)
Exit code 0 = all good, 1 = problems found.

Call sites recognised: t!("key"), and any helper whose name contains "tr"
(tr, tr_with, ...) called as helper("key", "english", ...). Keys are the
dotted lowercase literals; the English fallback (2nd arg) is also checked for
placeholders against the locale files.
"""
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SRC = HERE.parent / "src"
S = r'"((?:[^"\\]|\\.)*)"'
CALL = re.compile(r'(?<![\w.])(?:t!|[a-z_]*tr[a-z_]*)\(\s*' + S + r'(?:\s*,\s*' + S + ")?", re.S)
KEY = re.compile(r"^[a-z0-9_]+(\.[a-z0-9_]+)+$")
# Deliberately-nonexistent keys used by the fallback-helper unit tests.
TEST_ONLY = {"ui.modal.definitely_missing_key", "ui.room.__never_a_key__"}
PH = re.compile(r"%\{(\w+)\}")


def load(path):
    """Flat `dotted.key: "value"` layout (that is what rust-i18n reads here)."""
    out = {}
    for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        m = re.match(r'^([\w.]+):\s*"((?:[^"\\]|\\.)*)"\s*$', line)
        if not m:
            if line.startswith("_version"):
                continue
            print(f"{path.name}:{n}: unparsable line: {line!r}")
            out.setdefault("__bad__", []).append(n)
            continue
        if m.group(1) in out:
            print(f"{path.name}:{n}: duplicate key {m.group(1)}")
            out.setdefault("__bad__", []).append(n)
        out[m.group(1)] = m.group(2)
    return out


def main():
    en = load(HERE / "en.yml")
    de = load(HERE / "de.yml")
    used = {}
    for f in sorted(SRC.rglob("*.rs")):
        text = f.read_text(encoding="utf-8")
        for m in CALL.finditer(text):
            if KEY.match(m.group(1)) and m.group(1) not in TEST_ONLY:
                line = text.count("\n", 0, m.start()) + 1
                used.setdefault(m.group(1), []).append((m.group(2), f"{f.relative_to(SRC.parent)}:{line}"))
    problems = 0
    for k, sites in sorted(used.items()):
        for name, d in (("en", en), ("de", de)):
            if k not in d:
                print(f"MISSING in {name}.yml: {k}  ({sites[0][1]})")
                problems += 1
        if k in en and k in de and set(PH.findall(en[k])) != set(PH.findall(de[k])):
            print(f"PLACEHOLDER MISMATCH: {k}: en={sorted(set(PH.findall(en[k])))} de={sorted(set(PH.findall(de[k])))}")
            problems += 1
        for eng, where in sites:
            if eng is not None and k in en and set(PH.findall(eng)) != set(PH.findall(en[k])):
                print(f"PLACEHOLDER MISMATCH vs inline English: {k} ({where})")
                problems += 1
    for k in sorted(set(en) ^ set(de)):
        print(f"KEY ONLY IN ONE LOCALE: {k}")
        problems += 1
    unused = sorted(k for k in en if k not in used and k != "_version")
    print(f"{len(used)} keys used in src; {len(en)} en / {len(de)} de entries; {problems} problem(s)")
    print(f"{len(unused)} entries not referenced by a literal call site (informational): {' '.join(unused)}")
    if "__bad__" in en or "__bad__" in de:
        problems += 1
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
