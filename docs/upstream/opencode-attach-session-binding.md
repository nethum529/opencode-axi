# Draft upstream issue: attached TUI composer ignores the session binding

> Draft only. The opencode-axi captain will file this on `sst/opencode`.

## Suggested title

`opencode attach` renders a session but initializes the composer from client defaults

## Environment

- OpenCode 1.18.10
- `opencode attach http://127.0.0.1:4096/ --session <session-id>`

## Reproduction

1. Create a session whose stored fields bind it to agent `impl`, provider/model
   `opencode/deepseek-v4-flash-free`, and variant `high`.
2. Dispatch a message with that same per-request binding.
3. Attach from a client whose defaults are Build / `openai/gpt-5.6-terra` / `xhigh`.
4. Observe that the conversation header reflects Impl / DeepSeek while the composer status reflects
   Build / Terra / xhigh.
5. Type into the composer and observe that the new message uses the client defaults in the existing
   worker session.

## Expected behavior

The attached composer should adopt the persisted session agent/model/variant. Alternatively,
`opencode attach` should expose explicit `--agent`, `--model`, and `--variant` flags so callers can
bind the composer to the session before accepting input. If neither is possible, attach should be
read-only by default or visibly warn that the composer differs from the attached session.

## Actual behavior and impact

The session and conversation are attached correctly, but the composer silently uses unrelated
client defaults. This can contaminate a long-running worker session with a different agent, model,
and reasoning variant.

## Downstream workaround

opencode-axi labels headed tabs with the dispatched binding and `DO NOT TYPE: composer unbound`.
Operators send follow-up messages through oca until an upstream binding mechanism is available.
