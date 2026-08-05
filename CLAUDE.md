## Agent skills

### Issue tracker

Issues live as GitHub issues on `nethum529/opencode-axi`, managed via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context repo. `CONTEXT.md` points to the authoritative spec, which lives outside this repo in Obsidian. See `docs/agents/domain.md`.

## Build operations

- `codex-axi worker start` takes the **full** model id, not the short alias: `--model gpt-5.6-terra`,
  `--model gpt-5.6-sol`, `--model gpt-5.6-luna`. Passing `terra`/`sol`/`luna` fails with a
  misleading "not supported when using Codex with a ChatGPT account" error. That error is not an
  account restriction and not a CLI/SDK version mismatch. Verified live 2026-08-01 for all three.
- `codex-axi worker list` is unreliable for in-flight status (can report empty for a worker that
  is actually still running). Always cross-check `codex-axi worker view <id>` by its specific id,
  plus the worktree's own `git log`/`git status` and process liveness.
- A codex worker can die silently mid-task, leaving substantial uncommitted (but often buildable)
  work on disk. Never conclude "no progress" without checking `git diff --stat` in the worktree.
- New ticket work should land in its own module/file within a crate (e.g. `resolver.rs`,
  `error.rs`) rather than appended to a `lib.rs` another ticket already touched — two real merge
  conflicts this run came from exactly that pattern (`oca-state`, `oca-core`).
- `.claude/consult.enabled` gates this repo, but a codex-axi worker has no path to reach the
  consultant agent — only Claude-side dispatches actually go through consult checkpoints today.
  This is a known gap, not a design choice.
- `director` has standing authorization to spawn Fable agents on this repo, including `consult`
  and `fable-review`. Granted by the user 2026-08-01. Do not ask per spawn. Consult checkpoints
  are therefore real reviews from now on, not recorded misses.
