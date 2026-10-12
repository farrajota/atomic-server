# @tomic/lib: The Atomic Data library for typescript/javascript

[**docs**](https://docs.atomicdata.dev/js)

Local caching is optional for server-acknowledged saves. Background cache and
sync work waits for the captured identity database to open, and is cancelled
when that database or identity is replaced. Storage refusal leaves online
reads and saves available without claiming local durability.

Offline and local-only saves require a durable resource snapshot. They reject
when storage cannot open, the write fails, or its owner changes before
publication. Snapshot and database-backed outbox rows share the same durable
write; an unavailable cache is never a successful offline save.

