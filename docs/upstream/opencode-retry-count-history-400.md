# Draft upstream issue: persisted retryCount makes session history unreadable

> Draft only. The opencode-axi captain will file this on `sst/opencode`.

## Suggested title

Provider retryCount persisted inside message info.format causes the session message endpoint to 400

## Environment

- OpenCode 1.18.10
- A structured prompt using a JSON-schema output format

## Reproduction

1. Send a structured prompt whose stored user message has `info.format` containing the JSON-schema
   output description.
2. Cause the provider turn to retry.
3. Observe retry bookkeeping add `retryCount` inside the persisted `info.format` object.
4. Request `GET /session/<session-id>/message`.

## Expected behavior

Retry bookkeeping should not mutate the persisted output-format schema, and a session should always
be able to deserialize and return its own stored messages.

## Actual behavior

The endpoint returns HTTP 400 with a schema rejection similar to:

```text
Expected OutputFormatJsonSchema, got {...,"retryCount":2}
```

The attached TUI does not surface the failed history request and renders an empty, zero-token
conversation even though the prompt and provider turn occurred.

## Downstream workaround

opencode-axi probes the message endpoint around attach/follow. A rejected read creates an
`oca.history.unreadable` journal event and a visible headed-tab/follow warning. The workaround can be
removed once stored retry metadata no longer poisons history serialization.
