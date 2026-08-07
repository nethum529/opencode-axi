# Headed display verification checklist

Use this checklist for every live headed-dispatch probe. Deterministic integration tests remain the
required CI evidence; the live probe validates only the installed OpenCode/herdr boundary.

1. Dispatch a background headed worker with an explicit distinguishable model that still supports
   dispatch, for example `oca luna:h -b -w verify headed identity`. Do not use `flash`/DeepSeek for
   this probe: unsupported tool choice now fails before dispatch.
2. Capture the pane id (for example `w5:pV`) and rendered history with
   `herdr agent read <pane>` immediately after attach.
3. Assert the tab/window label is the task display name (for the example, `verifyHeadedIdentity`).
   For herdr, `herdr agent read <pane>` must contain OpenCode's rendered worker header (for example,
   `Impl · GPT-5.6 Luna`) and `herdr agent get <pane>` must return the compact slug name (for example,
   `w6h6mn-impl-luna-high`). For tmux, `tmux display-message -p '#{@oca-identity}'` must return the
   full identity string. The full binding printed by oca must name
   the ref, agent, provider/model, and variant explicitly.
4. Treat the OpenCode composer as unsafe for input while upstream attach cannot bind it. The label
   remains the task name, and the composer may still show client defaults. On both backends the oca
   binding says `DO NOT TYPE: composer unbound`; do not type in the composer. Use `oca m <ref> ...`
   or `oca q <ref> ...` instead.
5. When `GET /session/<id>/message` returns 200, assert the captured TUI contains the dispatched
   prompt or turn, the worker's model string, and ordinary conversational assistant prose before
   the final fenced `json` contract block. A zero-token empty conversation, a tool-call-only turn,
   or a contract block with no visible explanatory prose is a failure.
6. When that history request returns a non-200 response, assert `oca events <ref>` contains
   `oca.history.unreadable` and `oca f <ref>` prints the unreadable-history warning.
7. Assert the tab remains open through intermediate completed steps. The shared
   `[herdr].close_on_done` policy applies to both backends: when `true`, herdr closes its tab and
   tmux kills its window only for a `done` terminal state; `partial`, `blocked`, `failed`, and
   `unclear` stay open with their marker visible. When `false`, every terminal state stays open on
   both backends. `oca k <ref>` is the intended reaper for retained displays, so a tab strip that
   accumulates non-done workers is expected captain-action state, not a leak.
8. At the terminal idle boundary, assert the oca-owned surface exposes the outcome before any
   configured close. Herdr renames the compact slug with `-done`, `-part`, `-blkd`, `-fail`, or
   `-uncl` (truncating the base to 27 characters so the full slug remains at most 32 characters).
   Tmux resets `@oca-identity` to the full identity followed by ` | DONE`, ` | PARTIAL`,
   ` | BLOCKED`, ` | FAILED`, or ` | UNCLEAR`. A marker-write failure must remain visible in
   `oca events <ref>` as `oca.display.unmarked` even though the detached helper's stderr is hidden.

The default `[dispatch] transport = "text"` sends no prompt `format` field, so a fresh dispatch
cannot acquire OpenCode's `retryCount`-inside-format history poisoning. The retry-count purge and
history diagnostics remain necessary for legacy rows and for the one-release
`[dispatch] transport = "schema"` escape hatch, which intentionally preserves the old format
envelope.
