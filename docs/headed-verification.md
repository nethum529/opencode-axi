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
   prompt or turn and the worker's model string. A zero-token empty conversation is a failure.
6. When that history request returns a non-200 response, assert `oca events <ref>` contains
   `oca.history.unreadable` and `oca f <ref>` prints the unreadable-history warning.
7. Assert the tab remains open through intermediate completed steps and closes only at the terminal
   idle boundary (or explicit `oca k` when configured not to auto-close).
8. At the terminal idle boundary, assert the oca-owned surface exposes the outcome before any
   configured close: herdr renames the compact slug with `-done` or `-fail` (truncating the base so
   the full slug remains at most 32 characters), while tmux resets `@oca-identity` to the full
   identity followed by ` | DONE` or ` | FAILED`.
