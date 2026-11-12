# Changelog Maintenance

**Mandatory working rule for agents, linked (force-loaded) from the repo-root `CLAUDE.md`.**

`CHANGELOG.md` in the **repo root** is the user-facing "what changed". Write for users, in
plain language, not implementation detail.

## Plain language (hard rule)

Entries must be short and understandable by a user with **no technical knowledge**. Say
**what got better for the user, never how.** No file names, function names, flags, libraries,
protocols, config keys, commit ids or implementation details. One short sentence per bullet.
Drop anything a non-technical user would not notice. Example: a fix for a variable refresh
rate bug becomes `- **Variable refresh rate**: fixed an issue with variable refresh rate.`

## The shape

```
# Changelog

<short preamble naming the categories>

## YYYY-MM-DD

### Added
- **Short name**: one short sentence of what got better for the user.
```

- **`## YYYY-MM-DD`**: the date only, newest block on top. Date-based policy, no version
  numbers (see [`documentation-version-policy.md`](documentation-version-policy.md)).
- **`### Added` / `### Fixed` / `### Removed` / `### Info`**: the only four categories.
  *Added* = new features, *Fixed* = bug and behaviour fixes, *Removed* = things taken out,
  *Info* = notes worth knowing. Omit a category with no entries.
- **`- **Name**: sentence.`**: bold lead-in, colon, plain sentence. Wrap at about 90 columns,
  continuation lines indented two spaces.

## Rules

- **Maintain it as you work.** Every commit that changes what users get adds a bullet in
  the same commit, under **today's** `## YYYY-MM-DD` block. Create the block on top if it is
  missing; never create a second block for the same day.
- **Never rewrite a past block's date or entries.**
- **Skip internal-only changes**: docs, refactors, tests, CI and build tweaks add nothing,
  and a day of only such work adds no block.
- **Anonymize**: no hostnames, usernames, emails, real names, absolute home paths or private
  project names.
