# simple-screensharing

Planning workspace for a friends screensharing platform; see
`superdoc/architecture/overview.md`. This repo currently holds no source code — the
actual implementation lives in sibling repos (Rust relay server, Rust/egui desktop
client, browser-viewer site) that don't exist yet.

<!-- superdoc:start v2 -->
## Docs: read before you touch, update after you change

Full discipline — what to read, when to update, how to record the *why* — lives in the
always-loaded `superdoc/claude-instructions/documentation.md` below; this is a pointer, not
a restatement.

Reference docs — plain links, read the one relevant to your task on demand:

- **`superdoc/architecture/overview.md`** — codebase navigation map (module map, data flow,
  "where to look for X"). **Start here to navigate the code.**

## Always in context (force-loaded, mandatory)

`@` is reserved for the must-always-know set. Everything above is a plain link: an
optional, on-demand read.

- **Terminology** — @superdoc/meta/TERMINOLOGY.md — project vocabulary; use these meanings,
  ask before acting on an undefined term, and keep it current.
- **Documentation discipline** — @superdoc/claude-instructions/documentation.md — read
  before you touch, update after you change, record design rationale (the *why*).
- **Version policy** — @superdoc/claude-instructions/documentation-version-policy.md — how
  this project bumps doc/version markers.
- **Testing and verification** — @superdoc/claude-instructions/testing-and-verification.md —
  verify against the real app (not mock/browser) whenever a change touches real Tauri/Electron
  behavior; how to launch it safely.

Path base: `CLAUDE.md` uses repo-root-relative paths (`@superdoc/...`, `` `superdoc/...` ``);
links *inside* docs are docs-relative. Precedence: `CLAUDE.md` is authoritative for
rules, docs are the reference — reconcile to `CLAUDE.md` on conflict. `@`-budget:
force-load a file only if its absence would let the agent do the wrong thing on *any*
task.
<!-- superdoc:end -->
