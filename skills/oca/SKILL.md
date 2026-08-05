---
name: oca
description: Delegate engineering work to OpenCode workers and inspect or steer their state.
---

# oca

Dispatch every worker with an explicit alias and effort. There is no default.

Aliases and canonical effort ladders: luna: low medium high xhigh max; sol: low medium high xhigh max; terra: low medium high xhigh max; flash: high max; deepseek: high max.
Accepted effort spellings: l m h x max low medium high xhigh. Choose a spelling of a rung from the selected alias's ladder above.

Construct commands only from this parser-owned surface:

    oca <alias>:<effort> [--json] [-r|--role <role>] [-w|--worktree] [-b] [--headless] <prompt...>
    oca <alias> -e <effort> [--json] [-r|--role <role>] [-w|--worktree] [-b] [--headless] <prompt...>
    oca m <ref> [--json] [-e|--effort <effort>] <message...>
    oca q <ref> [--json] <message...>
    oca f <ref> [-t <seconds>] [--json]
    oca k <ref> [--json]
    oca ls [--all] [--blocked] [--count] [--json]
    oca events <ref> [--since <non-negative-integer>] [--json]
    oca push <ref> [--json]
    oca pr <ref> [--json]

Use the follow command when waiting for a worker. It exits 0 done, 3 blocked, 4 timeout, and 5 server unreachable.

Use the worktree option when edits must stay isolated. Workers never run git; oca validates the diff and commits locally after each worktree turn.

A blocked worker ends its turn with a question; answer it with the message command. Publication commands work only where the repository publish grant allows them. Merges and grants stay with the human.
