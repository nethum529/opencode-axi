## Agent skills

### Issue tracker

Issues live as GitHub issues on `nethum529/opencode-axi`, managed via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context repo. `CONTEXT.md` points to the authoritative spec, which is maintained outside this repo. See `docs/agents/domain.md`.

## Build operations

- New ticket work should land in its own module/file within a crate (e.g. `resolver.rs`,
  `error.rs`) rather than appended to a `lib.rs` another ticket already touched — two real merge
  conflicts came from exactly that pattern (`oca-state`, `oca-core`).
- A background worker can die silently mid-task, leaving substantial uncommitted (but often
  buildable) work on disk. Never conclude "no progress" without checking `git diff --stat` in the
  worktree.
