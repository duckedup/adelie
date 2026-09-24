# D0011: `ledger` is an engine in adelie, and transactions span every engine

**Status:** accepted · 2026-09-23
**Rule:** `ledger` is a table engine inside adelie, with a private WAL, `UPDATE`, and point
reads and writes. `BEGIN … COMMIT` spans any tables, of any engine, with one commit point.

## Why

- The maintainer chose `ledger` in adelie over a separate product (Q14).
- SPEC §18 puts commits in the core, so one transaction model serves every engine. A transaction
  limited to `ledger` tables could not also append to an event table atomically, and widening it
  later would mean redesigning the commit path. The maintainer chose "any tables".
- Build for the long term: this is settled before any engine but `append` exists.

## What it reverses

SPEC §2 listed "a write-ahead log" and "transactions, `UPDATE`, OLTP point workloads" as
non-goals. Now:

- A WAL exists only inside `ledger`. Every other engine keeps the no-WAL path (§6).
- Transactions are a core feature for every engine (§6).
- `UPDATE` and point workloads exist only on `ledger` tables.

## Consequences

- Q15 is open: how a WAL commit record and a manifest publish share one commit point, and
  how recovery replays exactly the committed transactions.
- Non-`ledger` writes in a transaction stay buffered until `COMMIT`, so a long transaction holds
  memory, and the memory budget has to account for it.
