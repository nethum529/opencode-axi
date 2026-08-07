# Headed display verification checklist

Use this checklist for every live headed-dispatch probe. Deterministic integration tests remain the
required CI evidence; the live probe validates only the installed OpenCode/herdr boundary.

1. Dispatch a background headed worker with a model whose display name is easy to distinguish from
   the local OpenCode default.
2. Capture the rendered tab with `herdr agent read <tab>` immediately after attach.
3. Assert the tab label names the dispatched worker's agent, full provider/model, and variant.
4. Treat the OpenCode composer as unsafe for input while upstream attach cannot bind it. The label
   must say `DO NOT TYPE: composer unbound`; use `oca m <ref> ...` or `oca q <ref> ...` instead.
5. When `GET /session/<id>/message` returns 200, assert the captured TUI contains the dispatched
   prompt or turn and the worker's model string. A zero-token empty conversation is a failure.
6. When that history request returns a non-200 response, assert the tab label says
   `HISTORY UNREADABLE`, `oca events <ref>` contains `oca.history.unreadable`, and `oca f <ref>`
   prints the unreadable-history warning.
7. Assert the tab remains open through intermediate completed steps and closes only at the terminal
   idle boundary (or explicit `oca k` when configured not to auto-close).
