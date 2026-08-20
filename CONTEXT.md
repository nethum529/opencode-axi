# oca (opencode-axi)

`oca` is a single Rust binary that gives an orchestrating AI agent a fast, token-minimal control surface over OpenCode workers through the local `opencode serve` HTTP API: four model aliases with mandatory per-request effort, a tiny fixed CLI grammar (`oca <alias>:<effort> "task"`, plus `m`/`s`/`q`/`f`/`k`/`ls`/`events`/`push`/`pr`), SSE-driven waiting with no polling and no daemon beyond `opencode serve` itself, tool-owned git (workers never run git; `oca` validates and commits), config-gated publish, and headed-by-default display via herdr with tmux and headless fallbacks.

The authoritative design and implementation spec are **not duplicated here** and are maintained outside this repo. They cover:

- current project state
- spec index, budgets, build-gate order, open verify items
- architecture — workspace, crates, OpenAPI generation, server discovery, SSE, herdr, git
- CLI surface — every verb, ack/error formats, worktree/commit/publish, generated deliverables
- data and state — config, refs, intents, journals, effort ladders, reply schemas
- risks — stress-test findings, ranked risk register
- testing — test layers, TUI-coexistence experiment, crash recovery
- binding design and spec-research decisions

Where this repo's code and the spec disagree, the spec is the contract; flag the conflict rather than silently resolving it.

Design intent that has been settled inside this repo is recorded in `docs/adr/`.
