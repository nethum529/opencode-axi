# oca (opencode-axi)

Rust agent-control surface for OpenCode workers. See design/spec notes in the project's Obsidian vault (OpenCodeAxi/) for the full contract.

## Default catalog provider limitation

The compiled `flash` alias (also available as `deepseek`) remains in the default catalog, but its `deepseek-v4-flash-free` provider currently exposes only thinking variants. The zen endpoint rejects tool use (`tool_choice`) whenever one of those variants is active, while `oca` dispatches are tooled and structured. Oca therefore rejects the default alias locally with `model_unsupported_tooled` before opening a server session or sending a prompt.

A `[models.flash]` configuration override defines a new catalog entry and does not inherit this compiled-in compatibility mark. The override is responsible for naming a provider/model that actually supports the tooled dispatch contract. See [`docs/upstream/deepseek-v4-flash-tool-choice.md`](docs/upstream/deepseek-v4-flash-tool-choice.md) for the provider report draft.
