# Live acceptance report — OpenCode 1.18.10

- Date: 2026-08-05
- Host: Linux 7.1.5-1-cachyos x86_64
- OpenCode: 1.18.10 (the ticket expected 1.18.8)
- herdr: 0.7.5

This is an explicitly invoked release suite. It is ignored by the hermetic workspace gate and
must be run with:

```sh
OCA_LIVE=1 cargo test -p oca-cli --test live_acceptance -- --ignored --nocapture --test-threads=1
```

The fixture starts and kills an OpenCode server for each test. Each server receives a temporary
`HOME`, XDG directories, repository, `.oca` state directory, configuration, and copied OpenCode
credentials. The invoking user's `~/.oca` is never read or written.

The Linux run passed all 12 executable tests in 116.15 seconds. A passing test can still record an
explicitly OPEN release criterion; the suite is intended to preserve measured facts without
hiding unavailable platforms, pre-T24 gaps, or server behavior that misses a target.

## Verification commands

| Command | Result |
|---|---|
| `OCA_LIVE=1 cargo test -p oca-cli --test live_acceptance -- --ignored --nocapture --test-threads=1` | PASS — 12 passed, 0 failed; 116.15 s |
| `cargo fmt --all --check` | PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS |
| `cargo test --workspace` | PASS (the 12 live tests remained ignored) |
| `cargo xtask check-drift` | PASS |
| `npm run check:plugin-runtime` | PASS |

## Ticket and live-spec acceptance

| Acceptance item | Named live test | Result and evidence |
|---|---|---|
| Session creation, HTTP prompt control, terminal SSE | `live_session_creation_http_prompt_terminal_sse_and_message_id_echo` | PASS — a real session completed an async HTTP prompt, emitted one observed `session.idle`, and exposed the terminal assistant reply in message history. |
| `variant` accepted on both prompt endpoints for both providers | `live_variant_is_accepted_on_both_prompt_endpoints_for_both_providers` | PASS — `variant=high` was accepted and echoed by `/prompt_async` and `/message` for `opencode/deepseek-v4-flash-free` and `openai/gpt-5.6-terra`. |
| Measure the DeepSeek variant's behavioral effect | `live_deepseek_variant_behavioral_effect_is_measured` | NO EFFECT — both variants were accepted and all 16 answers were correct, but the measured reasoning-token difference was not significant (`p=0.0777`). Follow-up: [#56](https://github.com/nethum529/opencode-axi/issues/56); it does not fail this suite. |
| Server-side JSON-schema output enforcement | `live_json_schema_output_enforcement_returns_conforming_reply` | PASS with OpenAI — the returned structured value was `{"code":37,"label":"LIVE-SCHEMA-OK"}`. DeepSeek thinking mode rejects this schema/tool-choice combination, so this provider-independent criterion uses OpenAI. |
| Deny-not-ask permission profile denies and returns | `live_permission_deny_returns_without_asking_or_hanging` | PASS — a worker attempted a denied `echo` command, returned in 4,632 ms, emitted zero permission-ask events, and did not create the sentinel file. |
| herdr workspace/tab/agent-start | `live_herdr_workspace_tab_agent_start_and_tui_boot_has_no_idle` | PASS — real herdr 0.7.5 created the workspace/tab and started agent `term_65854e1bc3dba8`; the tab was then closed. |
| herdr tab closes at terminal state | `live_queue_runs_after_turn_and_abort_reaches_terminal_for_tab_close` | PARTIAL/OPEN — a real headed tab remained present before `oca k`, abort reached a terminal idle event, and the fixture closed and verified the tab at that boundary. Automatic closure by the detached production attach helper is not proven on this pre-T24 branch. |
| `oca f` subprocess exits | `live_oca_follow_exit_gate_records_pre_t24_blocker_and_server_loss` | PARTIAL/OPEN — live server loss exits 5 with `server_unreachable`. Done, blocked, and timeout cannot be measured here because OpenCode 1.18.10 returns HTTP 400 (`Expected OutputFormatJsonSchema`) when `oca f` fetches history for schema-bearing turns. T24 is not present on this branch. |
| Warm invocation with attachment disabled under 10 ms | `live_warm_headless_ack_p95_is_measured_against_ten_millisecond_gate` | FAIL/OPEN — invocation-to-ack p50 was 36.402 ms and p95 was 83.066 ms, above the `<10 ms` target under investigation on T24. |
| Linux release platform | `live_platform_gate_runs_on_linux_or_macos` | PASS — the complete live suite passed on Linux x86_64. |
| macOS release platform | `live_platform_gate_runs_on_linux_or_macos` | OPEN — no macOS machine or runner is available; this criterion was not silently dropped. |
| Live `q` after the active turn | `live_queue_runs_after_turn_and_abort_reaches_terminal_for_tab_close` | PASS — two real legacy prompt admissions were server-serialized and both attributed replies appeared in order. Public `oca q` was also accepted against an active real session. |
| Live `k` and headed tab | `live_queue_runs_after_turn_and_abort_reaches_terminal_for_tab_close` | PARTIAL/OPEN — public `oca k` was accepted and the abort became terminal, but production-helper-driven tab closure remains unproven as described above. |
| Live `s` steer applied mid-turn | `live_steer_criterion_is_superseded_by_the_t18_scope_decision` | OPEN/SUPERSEDED — issue #18 removed `oca s` after the gate-0 experiment found OpenCode accepts but silently discards cross-pipeline steer. The retired CLI spelling now exits 2 (`invalid_model`). |
| TUI attach boot emits no spurious terminal event | `live_herdr_workspace_tab_agent_start_and_tui_boot_has_no_idle` | PASS — zero `session.idle` events were observed during a five-second real TUI-attach boot window. |
| Last-Event-ID/cursor replay | `live_last_event_id_replay_support_is_measured` | PASS as a measurement; answer NO — see the open-verify table below. |
| Caller message-id echo | `live_session_creation_http_prompt_terminal_sse_and_message_id_echo` | PASS as a measurement; answer YES — see the open-verify table below. |

## Open verify answers

| Item | Answer | Live evidence |
|---|---|---|
| #2: `session.idle` exactly once per turn and never on TUI boot | **NO to the strict formulation.** | A single isolated prompt emitted exactly one idle, and TUI boot emitted zero. However, two prompt admissions serialized while the session remained busy produced two attributed assistant replies followed by only one idle event for the overall busy span. OpenCode 1.18.10 therefore behaves as “once when the session becomes idle,” not reliably once per admitted turn. |
| #3: `Last-Event-ID`/cursor replay | **NO.** | Terminal frames carried no SSE `id` values (`sse_ids=[]`). A reconnect with `Last-Event-ID: oca-live-cursor-probe` replayed no old-session events, while the same stream did receive newly generated events. T17's messages-reconciliation fallback remains necessary. |
| #4: caller-supplied message id echoed | **YES.** | OpenCode stored the exact caller-supplied user message id and set the assistant message's `parentID` to it. Example run: session `ses_02bcdd1d1ffe6psLZj5vAmYK`, message `msg_fd4322e3a001hPonH5vAt7EDgu`. |

## DeepSeek behavioral-effect answer

Answer: **NO reliable effect in the release run.**

The test ran eight independent trials per variant with an exact two-sided permutation test over
reasoning-token counts. OpenCode echoed the requested variant on every response and all 16 answers
were correct.

| Variant | Reasoning-token samples | Mean |
|---|---|---:|
| `high` | 134, 224, 463, 254, 162, 218, 132, 153 | 217.5 |
| `max` | 142, 181, 94, 164, 230, 139, 115, 98 | 145.4 |

The exact permutation p-value was 0.0777. Earlier exploratory runs changed the direction and
significance of the difference, which reinforces that no repeatable behavioral effect has been
demonstrated. [Issue #56](https://github.com/nethum529/opencode-axi/issues/56) tracks investigation
of provider-side variant semantics and a more stable workload/effect metric; this ticket records
the required answer without blocking Linux.

## Warm timing

The timer begins before spawning `oca`, and stops after reading the acknowledgement's first line.
Attachment is explicitly disabled with `--headless`. The three warm-up samples are excluded.

Twenty measured samples in milliseconds, sorted:

```text
26.873, 32.559, 33.181, 33.378, 33.519, 34.045, 34.698, 34.911, 35.277, 35.333,
36.402, 37.584, 39.291, 39.642, 42.243, 44.197, 45.876, 46.588, 83.066, 167.757
```

Result: p50 36.402 ms; p95 83.066 ms; `<10 ms` gate FAIL/OPEN.

## Definition of done audit

| Definition-of-done item | Result | Test or evidence |
|---|---|---|
| Fake replay covers every OpenCode operation used and rejects everything else | PASS | `oca_testkit::tests::replay_matches_operation_and_canonical_json_then_preserves_sse_chunks`, `oca_testkit::tests::replay_rejects_undocumented_routes_and_unknown_json_fields`, and `oca_opencode::build_support::generates_only_the_facade_operations`. |
| Every public failure has a stable code, envelope, exit, and per-failure golden | PASS | `oca_core::error::tests::every_code_has_a_golden_envelope_and_frozen_exit_code`, `spec_conformance::every_spec_code_table_entry_has_an_error_code_and_frozen_exit_number`, the error-envelope schema, and subprocess envelope tests. |
| A killed follow leaves the worker recoverable | OPEN | T24 crash-recovery reconciliation is intentionally not merged into this branch. The live suite does not claim this pre-T24 behavior. |
| A lost HTTP response cannot duplicate a prompt | PASS | `failure_injection::post_transmit_pre_response_cut_never_replays_and_persists_unknown_ref` and `mid_sse_cut_keeps_the_admitted_prompt_exactly_once`. |
| Worktree validation cannot commit an out-of-scope or zero-byte path | PASS | `oca_git::tests::validate_rechecks_after_staging_and_rejects_a_late_out_of_scope_file`, `validate_rechecks_after_staging_and_rejects_a_manifest_file_emptied_late`, `validate_rejects_out_of_scope_paths_and_leaves_the_diff_intact`, and `validate_rejects_zero_byte_regular_files_by_name`. |
| 429 preserves retry metadata and never replays | PASS | `failure_injection::every_429_is_terminal_rate_limited_with_retry_metadata_and_no_replay`, `facade::every_operation_preserves_429_retry_metadata`, and `worktree_rate_limit_creates_no_commit`. |
| herdr absence leaves worker execution healthy | PASS | `headed_attach::no_herdr_and_no_tmux_is_a_silent_headless_http_dispatch`, `no_herdr_inside_tmux_creates_and_cleans_up_a_ref_named_window`, and `a_never_responding_herdr_socket_does_not_delay_dispatch_ack_or_completion`. |
| TUI contention experiment produced a documented policy | PASS | `docs/experiments/tui-coexistence.md`; issue #18 withdrew shared-input steer while preserving HTTP-authoritative queue behavior. |
| Live behavior matches pinned recordings | PARTIAL/OPEN | The live queue run exposed that legacy OpenCode serializes a second plain `prompt_async`; the facade and hermetic request assertions were corrected accordingly. Variant, permission, SSE, and replay assumptions match. Schema-bearing message-history reads still disagree with the generated client (`Expected OutputFormatJsonSchema`), blocking a complete match until T24/follow-up work. |

## Release status

The separate Linux live executable is green. The overall first-stable-release gate remains OPEN for
macOS, the pre-T24 follow/recovery criteria, production automatic headed-tab cleanup, the warm
timing target, and schema-bearing message-history compatibility. The
DeepSeek no-effect result is recorded in [issue #56](https://github.com/nethum529/opencode-axi/issues/56)
and is explicitly non-blocking by ticket direction.
