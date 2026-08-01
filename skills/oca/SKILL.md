---
name: oca
description: Delegate engineering work to OpenCode workers and inspect or steer their state.
---

# oca

Dispatch every worker with an explicit alias and effort. There is no default.

    oca luna:h "<task>"
    oca flash:h -r impl -w -b "<task>"

Aliases: luna, sol, terra (gpt-5.6), flash (deepseek-v4-flash-free).
Efforts: l m h x max. flash accepts h and x only.

Use -b for work that can run while you continue, then immediately park:

    oca f <ref>

The follow exits 0 done, 3 blocked, 4 timeout, 5 server unreachable.

    oca m <ref> "<message>"
    oca s <ref> "<message>"
    oca q <ref> "<message>"
    oca k <ref>
    oca ls
    oca events <ref>

Use -w when edits must stay isolated. Workers never run git; oca validates the
diff and commits locally after each worktree turn.

The impl reply is status, files, and a note of at most five sentences. A blocked
worker ends its turn with a question — answer it with `oca m <ref> "<answer>"`.

`oca push` and `oca pr` work only where the repository publish grant allows it.
Merges and grants stay with the human.
