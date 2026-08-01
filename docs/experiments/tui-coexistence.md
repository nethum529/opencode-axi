# TUI coexistence experiment — gate 0

Runs the experiment defined in `spec-testing.md` "TUI coexistence experiment" against real
infrastructure. Decides whether headed mode ships with **shared input** or degrades to
**observer-only**.

## Verdict

**Headed mode ships with shared input. The TUI may be attached to the same session the
orchestrator drives over HTTP, and the user may type into it.**

All six cases that test TUI/HTTP contention (1, 2, 3, 4, 7, 8) pass. Cases 5 and 6 fail, but they
do not fail on contention: the `delivery=steer|queue` control message is silently dropped by a
pipeline mismatch that is present with no TUI attached at all. Observer-only would not fix them.
The degradation path in `spec-testing.md` "If it fails" is **not** taken.

**Case 8 passes.** There is no spurious `session.idle` at TUI attach boot. The failure that would
have made every headed dispatch report `done` about three seconds in, commit an empty diff, and
return `worktree_empty` does not occur.

Two hazards found that are not TUI-specific and that gate-1 contracts must absorb:

1. A caller-supplied `messageID` that does not sort correctly against server-minted ids corrupts
   turn boundaries. See `gate0-verify.md` item 2.
2. `delivery=steer|queue` lives in a session runtime disjoint from the one the TUI and
   `prompt_async` use. See "Cases 5 and 6" below and `gate0-verify.md` "Additional findings".

## Environment

| Component | Value |
|---|---|
| OpenCode | **1.18.10** (spec pins 1.18.8; 1.18.8 was not installed, see Deviations) |
| herdr | 0.7.5, protocol 17, socket `/home/nethum/.config/herdr/herdr.sock` |
| Server | `opencode serve --port 4733 --hostname 127.0.0.1` |
| Model | `opencode/deepseek-v4-flash-free` |
| Project dir | `/home/nethum/opencode-axi-worktrees/T03` |
| Date | 2026-08-01 |

TUI attach used `opencode attach http://127.0.0.1:4733 -s <sessionID>` under a real PTY
(200x50, `TERM=xterm-256color`). TUI input was injected through the server's own
`POST /tui/append-prompt` + `POST /tui/submit-prompt`, which dispatch to the attached TUI over its
`/tui/control/next` long poll. TUI abort used a real `ESC` keystroke written to the PTY.

## Instrumentation

Two scratch harness modules, reproduced in `docs/experiments/harness/`:

- `oca.py` — HTTP client plus a background `GET /event` SSE reader that timestamps every frame
  against a monotonic clock and filters by session id.
- `tui.py` — spawns `opencode attach` under `pty.openpty()`, pumps the master fd on a thread,
  strips ANSI, and exposes the visible buffer plus a keystroke writer.

Turn attribution is evaluated the way `oca f` must evaluate it: an assistant message belongs to a
dispatch only if its `parentID` equals that dispatch's `messageID`.

## Case results

| # | Case | Expected | Observed | Result |
|---:|---|---|---|:--:|
| 1 | HTTP prompt, TUI idle | one turn, one terminal event, TUI displays it | 1 assistant message, 1 `session.idle` at 2381 ms, TUI buffer contained the reply | **PASS** |
| 2 | TUI prompt, no HTTP activity | one turn; parked follow must not attribute it | 1 new assistant message, `parentID=msg_fbeb68553001z43E916W3XxWvB`, 0 messages attributable to the parked dispatch's `messageID` | **PASS** |
| 3 | Alternating HTTP and TUI | strict ordering, no interleaving corruption | 6 prompts, 6 assistant turns, 6 `session.idle`, id order == returned order, 0 lost | **PASS** |
| 4 | Concurrent HTTP and TUI | serialized into two turns, or one cleanly rejected; never lost | both accepted (HTTP 204 / TUI 200), serialized into 2 turns, 2 `session.idle`, both replies correct | **PASS** |
| 5 | HTTP `steer` while TUI generating | steer applied to in-flight turn | accepted HTTP 200 with a minted id, `session.next.prompt.admitted` fired, but never applied, never stored, never executed | **FAIL** |
| 6 | HTTP `queue` while TUI generating | queued message runs after the turn | accepted HTTP 200, admitted, but never executed; only the essay turn ran | **FAIL** |
| 7 | TUI abort while parked follow connected | follow observes abort, exits cleanly, does not hang | ESC interrupted; follow saw `session.idle` after 9.03 s and exited; assistant carries `MessageAbortedError`; TUI showed `· interrupted` | **PASS** |
| 8 | TUI attach boot, no prompt pending | **no spurious `session.idle`** | 20 s attached: zero session-scoped events of any type, `session.idle` count 0 | **PASS** |

### Pass criteria from the spec

- Each prompt produces exactly one turn — holds (cases 1, 3, 4), **provided** message ids are
  server-minted or correctly ordered. See case 3 below.
- Each turn produces exactly one terminal result — holds, **except** that abort emits two
  `session.idle` frames ~8 ms apart. `oca f` must deduplicate.
- Message order is stable — holds with correctly ordered ids.
- No prompt is lost — holds in every case, including concurrent (case 4).
- Case 8 produces no terminal event — holds.
- Case 2's event is correctly not attributed to the dispatch's own message id — holds; `parentID`
  is the discriminator and it is reliable.

## Case 8 — dedicated pass/fail line

**PASS.** Attaching `opencode attach <url> -s <sid>` to an idle session and leaving it for 20
seconds produced **zero** session-scoped events, and in particular zero `session.idle`. Raw
observation: `session-scoped events in 20s: NONE`, `session.idle count: 0`, TUI process alive,
buffer rendered (20215 chars after ANSI stripping) showing the project path and
`• OpenCode 1.18.10`. A second independent attach run reproduced the same result.

Headed dispatch is therefore not at risk of a boot-time false `done`.

## Case 3 — the ordering hazard

The first run of case 3 used caller-supplied ids of the form `msg_0000000000000000000000c3NN`.
It preserved all six prompts in order and lost nothing, but produced only **4** assistant turns
instead of 6: two turns each answered two user prompts, replying `HTTP-1\nTUI-1` and
`HTTP-2\nTUI-2`. The returned id order did not equal sorted id order.

Re-run identically with server-minted ids: **6 users, 6 assistants, 6 `session.idle`, id order ==
sorted order, 1:1 prompt-to-turn**.

The merge was caused by the synthetic ids sorting before every TUI-minted id, not by TUI
contention. This is the same root cause as the loop documented in `gate0-verify.md` item 2, and it
is the single most dangerous thing found in this batch.

## Cases 5 and 6 — pipeline mismatch, not contention

`delivery` exists only on `POST /api/session/{sessionID}/prompt`, whose body is
`{id?, prompt:{text,...}, delivery?: "steer"|"queue", resume?}` and whose 200 response is
`{data: SessionInputAdmitted}`. Sending it against a TUI-driven in-flight turn returned HTTP 200
with a minted id and emitted `session.next.prompt.admitted` and `session.next.prompted`, both
carrying the `delivery` value. The message then vanished: it never appeared in
`GET /session/{id}/message` and its instruction never took effect.

Isolation run, no TUI attached at all, `POST /api/session/{id}/prompt` as the very first prompt of
a fresh session:

- HTTP 200, `admittedSeq: 1`, id minted.
- Generation genuinely happened — `session.next.text.started`, `session.next.text.delta`,
  `session.next.text.ended`, `session.next.step.started`, `session.next.step.ended`.
- `GET /session/{id}/message` returned **0 messages**.
- **No `session.idle` was ever emitted.**

So OpenCode 1.18.10 carries two disjoint session runtimes:

| | legacy | next / v2 |
|---|---|---|
| submit | `POST /session/{id}/prompt_async` | `POST /api/session/{id}/prompt` |
| read | `GET /session/{id}/message` | not visible there |
| terminal event | `session.idle` | `session.next.step.ended` |
| `delivery=steer\|queue` | absent | present |
| TUI drives | yes | no |

Mixing them silently drops the control message. A silent drop is worse than a clean error, because
`oca s` would return success while steering nothing.

**Legacy already provides queue semantics.** Sending `prompt_async` while a TUI turn is in flight
returned HTTP 204, serialized after the running turn, and executed correctly —
`LEGACY-QUEUE-MARK` appeared in the transcript after the essay turn, with 2 `session.idle`.

## Design commitments

1. **Headed-by-default ships with shared input.** T25/T26 need no change to their headed mode:
   the TUI is attached read-write to the orchestrated session. The observer-only fallback in
   `spec-testing.md` is not exercised and should be retained only as dead-lettered contingency.
2. **HTTP stays authoritative.** Unchanged, and now evidence-backed: `parentID` cleanly separates
   TUI-originated turns from dispatch-originated ones (case 2).
3. **`oca f` must deduplicate terminal events.** Abort emits two `session.idle` about 8 ms apart.
4. **`oca` must use the legacy pipeline throughout.** `prompt_async` for dispatch,
   `GET /session/{id}/message` for reconciliation, `session.idle` as terminal.
5. **`oca q` uses `prompt_async` while busy, not `delivery=queue`.** Verified to serialize.
6. **`oca s` has no server-side steer on the legacy pipeline.** `delivery=steer` is unreachable
   from a TUI-driven or `prompt_async`-driven session. T13 must either implement steer as
   abort-then-prompt or move the whole design to the v2 pipeline. This is a spec conflict, not an
   implementation detail — see the escalation note in `gate0-verify.md`.
7. **Message ids are constrained.** See `gate0-verify.md` item 2 for the binding rule.

## Deviations

- **OpenCode 1.18.10, not the pinned 1.18.8.** 1.18.8 is not installed on this machine and the
  ticket forbids substituting the fake server. All findings are therefore against 1.18.10 and
  should be re-confirmed if the project pins 1.18.8. Every finding here is structural (endpoint
  topology, id ordering, event taxonomy) rather than a tuning detail, so the risk of version skew
  changing a verdict is concentrated in cases 5 and 6, where the v2 pipeline may simply be newer
  than 1.18.8.
- Case 8 was observed for 20 s per run rather than an open-ended soak.
- TUI input for cases 1-6 was injected via `/tui/append-prompt` + `/tui/submit-prompt` rather than
  synthesized keystrokes. Case 7 used a genuine `ESC` keystroke through the PTY. A contention bug
  reachable only through the TUI's own keystroke handling would not be caught by cases 1-6.
