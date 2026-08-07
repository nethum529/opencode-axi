# Upstream report draft: DeepSeek Flash rejects `tool_choice` in every variant

Status: draft for the captain to file with the zen/OpenCode provider.

## Summary

The `deepseek-v4-flash-free` model rejects a tooled request with HTTP 400:

```text
Thinking mode does not support this tool_choice
```

The rejection occurs for every server-advertised variant: `low`, `high`, and
`max`. The variants are all reasoning/thinking variants, and the endpoint does
not expose a variant-free mode that can accept tool use.

## Reproduction

Against a local OpenCode server backed by the zen provider:

1. Create a session for `deepseek-v4-flash-free`.
2. Submit a prompt with one of `variant=low`, `variant=high`, or `variant=max`.
3. Include the agent's tools and a structured JSON output format.

Each variant is rejected before a tooled turn can complete. Changing the
reasoning effort does not avoid the failure.

## Expected behavior

Either expose a non-thinking `deepseek-v4-flash-free` variant that accepts
`tool_choice`, or document that tool use is unsupported and return a stable
capability error before dispatch. A structured-output request should not be
left as a provider 400 after a session/tab has already been admitted.

## Oca mitigation

Oca keeps the `flash`/`deepseek` aliases in its default catalog for discoverability,
but marks the compiled entry as incompatible with tooled dispatch and fails
before contacting the provider. A user configuration that replaces the alias
gets a fresh, unmarked catalog entry.

The catalog ladder remains `high, max`; it was not aligned to the server's
additional `low` variant. `low` is also a thinking variant and would still be
rejected, so adding it would advertise another unusable rung without changing
the fail-fast outcome.
