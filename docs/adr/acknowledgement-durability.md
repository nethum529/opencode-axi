# Acknowledgement durability boundary

- Status: Accepted
- Date: 2026-08-02
- Decision owner: D-ACK (#31)

## Context

The data-state specification describes a ref replacement as a temporary-file
write, `fsync`, and atomic rename, but also says that `fsync` is outside the
acknowledgement critical path even though acknowledgement follows the rename.
The architecture specification separately forbids `fsync` before
acknowledgement. The two statements do not distinguish syncing the temporary
file from syncing the directory that contains the renamed entry.

The distinction matters. A successful temporary-file sync before rename makes
the replacement's contents stable. An atomic rename then ensures readers see a
complete old file or a complete new file. Syncing the parent directory is what
makes that rename itself survive power loss. Requiring both syncs before the
acknowledgement gives the strongest guarantee, but puts both storage barriers
on the dispatch-latency path. Moving both barriers after acknowledgement can
leave a renamed file with incomplete contents after power loss.

The minimum acceptable guarantee is crash consistency: after recovery, a ref
is either fully present and valid or absent; it is never torn. Acknowledgement
does not promise that the newly allocated ref will survive immediate power
loss. The session remains recoverable from the OpenCode server if that mapping
is absent.

## Decision

Acknowledgement occurs after atomic replacement of `refs.json`, but before the
parent-directory sync. The exact order is:

1. While holding the exclusive `refs.lock`, create the temporary file and write
   the complete serialized replacement to it.
2. Sync the temporary file and require that sync to succeed.
3. Atomically rename the synced temporary file over `refs.json` and require the
   rename to succeed.
4. Emit and flush the dispatch acknowledgement.
5. After acknowledgement, sync the parent directory containing `refs.json`.
6. Release `refs.lock` only after the directory-sync attempt has either
   succeeded or its failure has been surfaced and transferred to retry
   ownership as described below.

Steps 1 through 4 are the acknowledgement path. Step 5 is deliberately
deferred until after acknowledgement. The temporary-file sync and the
parent-directory sync are different operations and must not be described as a
single undifferentiated `fsync`.

At the instant acknowledgement is emitted, the new file is complete and is the
visible directory entry, but the rename is not yet guaranteed durable. If
power is lost at that instant, recovery may expose either the previous valid
`refs.json` or the complete replacement. Therefore the acknowledged ref may be
absent after recovery, but it must never be torn or partially valid.

## Deferred completion and errors

The process that performed the rename owns the first post-ack directory-sync
attempt and keeps `refs.lock` for that attempt. A directory-sync failure after
acknowledgement cannot revoke the acknowledgement or turn it retroactively into
a synchronous dispatch failure. It is surfaced as a post-ack durability
warning on stderr and leaves directory durability pending.

If the attempt fails, or if the dispatch process exits after acknowledgement
without completing it, the next `oca` process that enters the ref store owns
the retry. That process retries the parent-directory sync while holding
`refs.lock`, before it relies on or performs another ref mutation. This
ownership rule applies after normal exit, error exit, or abrupt process death;
the operating system's release of the advisory lock permits the next process
to take ownership. A retry failure is surfaced to that process as a ref-store
durability error and remains retryable by the next entrant.

The contract above specifies observable ordering, ownership, and error
semantics. It does not select a thread, helper, marker, API, or other code
mechanism for implementing them.

## Consequences

The warm acknowledgement path pays for one content barrier, the temporary-file
sync, but not for the parent-directory metadata barrier. This keeps the minimum
work required for crash-consistent replacement on the critical path while
deferring the barrier that only strengthens the promise from old-or-new to
new-mapping-survives. The warm dispatch target remains a performance target to
measure; it is not permission to acknowledge potentially torn state.

Compared with acknowledging only after the directory sync, this decision
reduces acknowledgement latency at the cost of allowing the new ref mapping to
be absent after immediate power loss. That loss is recoverable from the server
and is already within the stated minimum guarantee.

Compared with acknowledging before the temporary-file sync, this decision
retains a content barrier before rename. A rename-before-content-sync design
could recover a torn `refs.json`; an acknowledgement-before-rename design would
make the acknowledged ref temporarily unusable and require a separate durable
handoff for the replacement itself. Either alternative drops below the chosen
acknowledgement contract to save the remaining required storage barrier.

## Follow-up boundary

FIX08 (#36) must use this ADR as the decision from which its acceptance
criteria are authored. This ADR intentionally does not prescribe FIX08's
implementation or any code change.
