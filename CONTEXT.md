# oca (opencode-axi)

`oca` is a single Rust binary that gives an orchestrating AI agent a fast, token-minimal control surface over OpenCode workers through the local `opencode serve` HTTP API: four model aliases with mandatory per-request effort, a tiny fixed CLI grammar (`oca <alias>:<effort> "task"`, plus `m`/`s`/`q`/`f`/`k`/`ls`/`events`/`push`/`pr`), SSE-driven waiting with no polling and no daemon beyond `opencode serve` itself, tool-owned git (workers never run git; `oca` validates and commits), config-gated publish, and headed-by-default display via herdr with tmux and headless fallbacks.

The authoritative design and implementation spec are **not duplicated here**. They live in the Obsidian vault:

- `OpenCodeAxi/_state.md` — current project state
- `OpenCodeAxi/spec/_spec-index.md` — spec index, budgets, build-gate order, open verify items
- `OpenCodeAxi/spec/spec-architecture.md` — workspace, crates, OpenAPI generation, server discovery, SSE, herdr, git
- `OpenCodeAxi/spec/spec-cli-surface.md` — every verb, ack/error formats, worktree/commit/publish, generated deliverables
- `OpenCodeAxi/spec/spec-data-state.md` — config, refs, intents, journals, effort ladders, reply schemas
- `OpenCodeAxi/spec/spec-risks.md` — stress-test findings, ranked risk register
- `OpenCodeAxi/spec/spec-testing.md` — test layers, TUI-coexistence experiment, crash recovery
- `OpenCodeAxi/decisions/oca-design-decisions.md` and `OpenCodeAxi/decisions/oca-spec-research-decisions.md` — binding decisions

Read the relevant spec note before touching an area of this codebase. Where this repo's code and the spec disagree, the spec is the contract; flag the conflict rather than silently resolving it.
