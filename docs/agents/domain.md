# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root — a pointer, not the spec itself. The authoritative design and spec live outside this repo, in the Obsidian vault under `OpenCodeAxi/` (see `CONTEXT.md` for the note names).
- **`docs/adr/`** — read ADRs that touch the area you're about to work in.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront.

## File structure

Single-context repo:

```
/
├── CONTEXT.md
├── docs/adr/
└── crates/
```

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in the Obsidian spec (`OpenCodeAxi/spec/`) — e.g. `ref`, `intent`, `worktree`, `role`, `effort`/`variant`, `follow`, `spawner tag`. Don't drift to synonyms the spec avoids.

## Flag ADR / spec conflicts

If your output contradicts an existing ADR in `docs/adr/` or a binding decision in the Obsidian decision logs (`OpenCodeAxi/decisions/`), surface it explicitly rather than silently overriding.
