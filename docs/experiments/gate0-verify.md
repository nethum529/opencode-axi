# Gate-0 verification batch — items 2, 3, 4

Resolves `_spec-index.md` open verify items #4, #5 and #6 against real infrastructure, before
gate-1 contracts freeze on them. Item 1 of the ticket (the TUI coexistence experiment) is in
`tui-coexistence.md`.

## Environment

| Component | Value |
|---|---|
| OpenCode | **1.18.10** (spec pins 1.18.8; not installed, see Deviations) |
| herdr | 0.7.5, protocol 17, socket `/home/nethum/.config/herdr/herdr.sock` |
| Server | `opencode serve --port 4733 --hostname 127.0.0.1` |
| Global permission config | `{"*": "allow"}` |
| Date | 2026-08-01 |

## Answers

| # | Question | Answer |
|---:|---|:--:|
| 1 | Is a per-session deny-not-ask permission profile expressible at dispatch, and does it deny rather than ask? | **YES** |
| 2 | Does OpenCode echo a caller-supplied `message_id` on prompt submission? | **YES, with a binding ordering constraint** |
| 3 | Does `variant` change behavior on `deepseek-v4-flash-free`, not merely get accepted? | **NO** |

---

## Item 1 — Deny-not-ask permission profile: YES

`spec-risks.md` risk #10 / assumption #4 is **confirmed**. The headless-worker design is safe. No
escalation wall.

### Expressibility

`POST /session` accepts `permission: PermissionRuleset`, an array of
`{permission: string, pattern: string, action: PermissionAction}` where `PermissionAction` is
exactly `["allow", "deny", "ask"]`. The ruleset is echoed back on the created session verbatim.

### Behavior — blanket deny

Session created with `[{"permission":"bash","pattern":"*","action":"deny"}]`, then prompted to run
a shell command.

- `session.idle` at 3376 ms. Session did not hang.
- **0** `permission.asked` and **0** `permission.v2.asked` events.
- The model replied: `"I don't have a bash tool available in this environment. My available tools
  are limited to file operations (read/write/edit), search (glob/grep), web (webfetch/websearch),
  task delegation, and question"`.

A blanket deny removes the tool from the exposed toolset entirely, so the model never attempts it.

### Behavior — pattern-scoped deny, tool still present

Stronger test, so the deny is exercised at call time rather than by tool elision. Ruleset
`[{"permission":"bash","pattern":"*","action":"allow"}, {"permission":"bash","pattern":"echo *","action":"deny"}]`,
prompted to run `echo hello-from-bash`.

- `session.idle` at 5549 ms. Session did not hang.
- **0** permission-ask events of either generation.
- The tool part came back with `status: "error"` and error text
  `"The user has specified a rule which prevents you from using this specific tool call. Here are
  some of the relevant rules [...]"`.
- The model then reported: `"The command was denied by a permission rule (bash:echo * → deny), so
  no output was produced."`

**Deny denies.** It does not ask, does not emit a permission request, and does not park the turn.
Both permission event generations (`permission.asked` and `permission.v2.asked`) were monitored and
neither fired.

### Behavior — the denied action produces no side effect

Reporting a deny while executing anyway is the worst false pass available here, and the two tests
above cannot detect it: both read only the reported outcome. Re-run with an observable side effect,
and with a positive control so the test is known to be capable of detecting execution at all.

Each trial prompts the worker to run `echo SENTINEL > <path>` through bash, then checks the
filesystem directly.

| Trial | Ruleset | Tool call outcome | Sentinel file |
|---|---|---|:--:|
| positive control | `bash *` allow | `status: "completed"` | **created** |
| blanket deny | `bash *` deny | no tool call attempted | **absent** |
| pattern-scoped deny | `bash *` allow, `bash echo *` deny | `status: "error"`, rule cited | **absent** |

The positive control created the file, so the assertion can observe execution. Neither deny variant
produced the side effect, and neither emitted an ask event. **Deny prevents the action, it does not
merely report having prevented it.**

### Consequence

T04 and T13 may proceed on the deny-not-ask assumption. Workers dispatched with a restrictive
per-session ruleset fail closed and terminate, rather than hanging on an unanswerable prompt.

---

## Item 2 — Caller-supplied message id echo: YES, with a binding constraint

`spec-risks.md` hardest-problem #1 residual is **resolved in favour of the design**, but the
constraint below is mandatory and its violation is severe.

### Echo confirmed

Both `POST /session/{id}/prompt_async` and `POST /session/{id}/message` accept `messageID`,
schema-typed `{"type":"string","pattern":"^msg"}`. Submitting
`messageID: "msg_oca_t03_idem_0001"` and reading `GET /session/{id}/message` returned the user
message with `id: "msg_oca_t03_idem_0001"` verbatim, and the answering assistant message carried
`parentID: "msg_oca_t03_idem_0001"`.

T24 can therefore reconcile on an id it minted itself, and `parentID` gives exact terminal
attribution — the property `tui-coexistence.md` case 2 depends on.

### Echo confirmed where attribution actually reads it — the `/event` stream

The response body and `GET /session/{id}/message` are not where `oca f` reads attribution; it
terminates a follow on the assistant message whose parent is the submitted id, observed over
`GET /event`. Asserted directly against the event frames, submitting
`messageID: "msg_fbec00000001oCaT03EchoTest0"`:

- The caller-minted id appears as the user message id in `message.updated` frames on `/event`.
- Every assistant `message.updated` frame carried `parentID` equal to the caller-minted id — 3 of 3
  frames, one distinct parent id observed, no unattributed frame.
- One `session.idle`.

The parent linkage is therefore reliable in the transport the design actually consumes, not only in
the REST read model.

### The constraint — ids must sort correctly

OpenCode mints ids as `msg_` plus a lexicographically time-ordered suffix (`msg_fbea978c2001…`,
`msg_fbeb04c2c001…`). Turn termination depends on that ordering. A caller id that sorts **after**
the assistant messages that answer it makes the server treat the prompt as perpetually unanswered
and regenerate forever.

Controlled results, identical prompt and model, one prompt each:

| `messageID` | Outcome |
|---|---|
| *(omitted)* | terminates, `session.idle` at 2683 ms, **1** assistant message |
| `msg_fbeb00000001aaaaaaaaaaaaaaaa` | terminates, `session.idle` at 2998 ms, **1** assistant message |
| `msg_00000000000000000000000000` | terminates, **1** assistant message |
| `msg_zzzzzzzzzzzzzzzzzzzzzzzzzz` | **loops**, no terminal event, 10 assistant messages before abort |
| `msg_oca_t03_idem_0001` | **loops**, no terminal event, 32 assistant messages before abort |

The runaway is not model-specific, agent-specific, or `variant`-specific. It reproduced on
`opencode/deepseek-v4-flash-free` and `openai/gpt-5.6-terra-fast`, on agents `build` and `plan`,
with and without `variant`. Control: plain `opencode run` with no caller id returned one reply and
exited 0.

During a runaway the session reports `session.status: busy` continuously and emits **no**
`session.idle`. It stops only on `POST /session/{id}/abort`, which is effective and immediate —
message count froze at 32 and stayed there.

A second, quieter symptom of the same cause: ids that sort *before* existing messages corrupt
ordering rather than looping. See `tui-coexistence.md` "Case 3 — the ordering hazard", where
low-sorting ids merged six prompts into four turns.

### Binding rule for T24

A caller-minted `messageID` **must** be time-ordered consistently with OpenCode's own scheme, so
that it sorts after every existing message in the session and before every message the server will
mint in response. Deriving the suffix from the same monotonic clock source and width as
`msg_fbeb…` satisfies this; an arbitrary readable slug such as `msg_oca_<ref>_<n>` does **not**
and will hang the worker in an unbounded, unterminated generation loop.

This must be covered by a unit contract and by a fake-server replay case. It is a silent,
expensive failure: no error, no terminal event, unbounded token spend.

---

## Item 3 — `variant` behavioral effect on `deepseek-v4-flash-free`: NO

`spec-risks.md` assumption #2 is **not** confirmed. `variant` is accepted and faithfully echoed,
but it does not measurably change behavior on the free provider.

### Accepted and echoed

`GET /config/providers` declares `opencode/deepseek-v4-flash-free` with variants exactly
`["high", "max"]` — consistent with the spec's `flash` ladder rejecting `l`/`m` and offering
`h`/`x`. Every request echoed its variant back on the assistant message `info.variant`, and on the
session `model.variant`. Acceptance was never in doubt; the question was effect.

### Measurement

Identical prompt (an exact-fraction probability question requiring reasoning), fresh session per
trial, n = 8 per variant, `deepseek-v4-flash-free`.

| variant | reasoning tokens | mean | sd | output mean | correct |
|---|---|---:|---:|---:|:--:|
| `high` | 94, 161, 201, 0, 116, 216, 94, 0 | 110.2 | 76.5 | 92.9 | 8/8 |
| `max` | 120, 100, 174, 157, 127, 103, 152, 106 | 129.9 | 26.1 | 110.0 | 8/8 |

Two-sided permutation test, 200 000 resamples:

- reasoning tokens: difference 19.6, **p = 0.53**
- output tokens: difference 17.1, **p = 0.056**

Neither reaches significance. Within-variant spread for `high` (0 to 216) exceeds the
between-variant difference. Answer quality is identical: 8/8 correct on both.

The only suggestive signal is that `high` returned `reasoning: 0` twice while `max` never fell
below 100, hinting at a reasoning floor under `max`. At n = 8 this is not a result, and it does not
affect answer quality.

### Consequence

The `flash` alias ladder is being built on a distinction the free provider does not honour.
`variant` is safe to send — it is accepted, echoed, and harmless — so the alias grammar and the
effort matrix can ship unchanged, and the `h`/`x` ladder remains correct as a *contract*. But no
part of the design may promise the user a behavioral difference between `flash:h` and `flash:x`,
and no test may assert one. T29's release-time re-measurement should treat a positive result as the
change, not this negative as the baseline to defend.

---

## Additional findings

These were not on the ticket but fall inside the gate-0 blast radius and are load-bearing for
gate-1 contracts.

### Two disjoint session runtimes

This is the single root cause of both TUI-coexistence case 5 and case 6. Those cases are not
independent failures and are not contention failures: one defect, observed twice.

OpenCode 1.18.10 exposes a legacy pipeline and a `next`/v2 pipeline that do not share state.
`POST /api/session/{id}/prompt` (the only endpoint carrying `delivery: "steer"|"queue"`) generates
text and emits `session.next.*` events, but writes nothing readable through
`GET /session/{id}/message` and emits no `session.idle`. Full evidence and the comparison table are
in `tui-coexistence.md` "Cases 5 and 6".

**Escalation note.** `oca s` (steer) as specified in `spec-cli-surface.md` is **not implementable
on the legacy pipeline**. `delivery=steer` is accepted with HTTP 200 and then silently dropped. T13
must choose between implementing steer as abort-then-reprompt, or migrating the whole design onto
the v2 pipeline and giving up `session.idle` and `GET /session/{id}/message`. This is a spec
conflict and is flagged rather than resolved here, per `CONTEXT.md`.

`oca q` (queue) is unaffected: `prompt_async` submitted while a turn is in flight returns 204,
serializes behind the running turn, and executes.

### Abort emits duplicate terminal events

`POST /session/{id}/abort` produced two `session.idle` frames about 8 ms apart (observed at 36018
ms and 36026 ms on the same monotonic clock). The spec already requires "duplicate terminal events
produce exactly one report and one exit"; this confirms the requirement is live, not theoretical.

### `prompt_async` returns no body

`POST /session/{id}/prompt_async` returns **204 with an empty body**. The caller cannot learn a
server-minted message id from the response, which is precisely why item 2's caller-supplied id
matters — and why the ordering constraint in item 2 is not optional. By contrast
`POST /api/session/{id}/prompt` returns the minted id in `data.id`, but belongs to the other
pipeline.

## Deviations

- **OpenCode 1.18.10, not the pinned 1.18.8**, which is not installed on this machine. Per the
  ticket the fake server was not substituted. Items 1 and 3 are behavioral and unlikely to be
  version-sensitive. Item 2's ordering constraint is structural. The disjoint-pipeline finding is
  the most likely to differ on 1.18.8, where the v2 pipeline may be less complete or absent.
- Item 3 used n = 8 per variant on a single prompt. A wider prompt set could still surface an
  effect; this result rules out a large or reliable one, not any effect whatsoever.
