# Step 3: persist across reloads

The Store keeps resources in memory.
Durable storage in the browser comes from the **client database**: the same Rust store that powers AtomicServer, compiled to WASM, writing an encrypted file to the Origin Private File System (OPFS).
It runs in a dedicated worker; one tab per origin owns the file and the others talk to it over a `BroadcastChannel`, so several tabs share one database.

## Where the WASM comes from

The worker script ships inside `@tomic/lib`.
The WASM module does not, yet: it is built from the repository's `wasm/` crate and served by your app as static files.
Build it once and copy the two output files into your public folder:

```sh
# in a checkout of atomic-server
cd wasm
wasm-pack build --target web --out-dir pkg
cp pkg/atomic_wasm.js pkg/atomic_wasm_bg.wasm /path/to/your-app/public/wasm/
```

Publishing this as a package so the step becomes an `npm install` is on the [roadmap](../roadmap.md).

## Confirm the actual database open

The preceding [Store and Drive step](3-store-and-drive.md) requires SDK support for parentless no-server creation, attaches the `ClientDbWorker`, calls `init()`, and waits for `clientDb.whenOpen()` before creating or saving the local Drive and Book. This step explains the persistence that setup provides: `init()` starts setup, while only a successful `whenOpen()` confirms that the database is actually usable. It does not establish the SDK's separate creation capability.

`clientDb.isOpen` reports database-open state. `clientDb.isReady` is different: it also waits for bootstrap seeding. A seed producer must wait for actual open, not readiness that includes the seed it has yet to produce. If a supported database opens later, optional cache/index work may resume only for the current worker and agent; it does not make a refused or replaced worker's writes durable.

When the browser refuses local storage or the required APIs are unavailable, the Store can still read from the server, but there is no local cache or offline durability. Local-only and offline saves require a successfully opened database; there is no in-memory fallback that promises reload persistence.

## What changes

- **Reads hit the database first.** `getResource` returns the stored copy without a network request, and only fetches when the resource is not present locally.
- **Writes are persisted with their history.** Each saved Resource's Loro snapshot is stored next to its properties, so version history survives a reload. Offline edits are persisted with their applicable outbox entry for a later reconnect drain.
- **Queries run locally.** `store.queryLocalDb` and `store.search` work against the local database, with the same index the server uses.

Reload the page: the Drive and Book are still there. Because the Drive is local-only, their saves are not in the server-drain outbox until you promote the Drive.

## What you have

A single-device app that survives reloads, with an encrypted local database and version history, and still no server.
The next step adds one, so the reading list can reach a second browser and a colleague.

Next: [syncing to an always-on device](5-sync-server.md).
