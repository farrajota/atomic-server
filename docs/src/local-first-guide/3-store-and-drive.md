# Step 2: a Store and a Drive, with no server

A `Store` is the in-memory graph your app reads and writes.
It used to need a `serverUrl`; now that is optional.
Give it the signing Agent from step 1 and it can create, sign and edit data on its own, provided the SDK supports explicitly parentless creation without a server URL.

This no-server flow requires that SDK capability as well as an actually opened client database. An SDK that still tries to normalize a missing `serverUrl` during parentless creation cannot run the Drive example below; initializing storage does not fix that creation limitation.

```ts
import { ClientDbWorker, Store, core } from '@tomic/lib';
import workerUrl from '@tomic/lib/client-db.worker.js?url'; // Vite syntax

const store = new Store({ agent });
```

Before creating the Drive or saving a Book, configure and open the client database as shown in [the persistence step](4-persist.md). A local-only or offline save requires a real usable database; attaching a worker or starting `init()` alone does not prove that storage opened. Do not proceed to the first required save until the actual-open check succeeds.

## Initialize and check local storage first

Build and serve the WASM module as described in [the persistence step](4-persist.md), then attach the database before creating local data:

```ts
const clientDb = new ClientDbWorker(
  `${location.origin}/wasm/atomic_wasm.js`,
  workerUrl,
  {
    // One file per Agent, so two identities on the same origin never share a
    // cache. Derive the name from the subject however you like.
    dbName: `reading-list.${hash(agent.subject)}.redb`,
    // Keep the key with the secret; without it the file is unreadable.
    dbKey: loadOrCreateDbKey(agent.subject),
  },
);

store.setClientDb(clientDb);
await clientDb.init();

if (!(await clientDb.whenOpen())) {
  throw new Error(clientDb.initError?.message ?? 'Local database did not open');
}
```

`whenOpen()` checks actual database open, not overall readiness that may also depend on bootstrap seeding. If a seed producer waits for `isReady`/seeded readiness while it is responsible for completing that seed, it waits on itself. This guide has no seed step; it waits only for the database to open.

Only after this check succeeds, and with SDK support for parentless no-server creation, should the examples below create and save the local Drive and Book. If storage is unsupported or denied, stop before those required local-only saves; there is no optional in-memory durability fallback.

## Create a Drive

A [Drive](../hierarchy.md) is the top-level container: the unit that gets shared, synced and backed up.
Every Agent has a *personal* Drive whose identifier is derived from the Agent's key, so the same identity gets the same Drive on every device without anything to look up.

```ts
const drive = await store.createDrive('Reading list', { localOnly: true });
store.setDrive(drive.subject);

console.log(drive.subject); // atomic:…
```

`localOnly: true` tells the sync engine to leave this Drive alone even if a server is connected later.
You will lift that in step 4.
Without it, the Store would try to offer the Drive to whatever server it connects to, which is the right default for an app that always has one.

## Create and edit Resources

A Book is a Resource with a name and a description.
This guide reuses two Properties from the core vocabulary so it needs no schema of its own; a real app defines its own Classes and Properties in the [ontology editor](../atomicserver/gui.md) and generates types with [`@tomic/cli`](../js-cli.md).

```ts
async function addBook(title: string, note: string) {
  const book = await store.newResource({
    parent: drive.subject,
    propVals: {
      [core.properties.name]: title,
      [core.properties.description]: note,
    },
  });

  await book.save();

  return book;
}

const book = await addBook('The Dispossessed', 'Recommended by Anna');
console.log(book.subject); // atomic:… (a fresh identifier, minted here)
```

`newResource` mints a `atomic:` identifier from a genesis certificate signed by your Agent.
On this local-only Drive, `save()` persists a durable Resource snapshot in the already-open client database without enrolling the edit in the server-drain outbox. The Promise resolves after the required snapshot write; if that write fails, it rejects and the in-memory edit remains unsaved.

Editing is the same call:

```ts
book.set(core.properties.description, 'Recommended by Anna. Finished it in a weekend.');
await book.save();
```

## Read and subscribe

```ts
const same = await store.getResource(book.subject);
console.log(same.get(core.properties.name)); // The Dispossessed

const unsubscribe = store.subscribe(book.subject, updated => {
  render(updated.get(core.properties.description));
});
```

`subscribe` fires for local edits now, and for edits arriving from other devices once you sync.
The callback does not care which.

## What you have

An identity, a Drive and a Book, all signed and created without a network request. Because the client database was opened before the first save, their snapshots survive reloads.
The Drive is local-only, so its saves are not sent to a server or queued for drain until you promote it.

Next: [persisting across reloads](4-persist.md) explains the client database this step already opened and what it persists.
