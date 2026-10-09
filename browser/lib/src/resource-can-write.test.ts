import { describe, it } from 'vitest';
import { Resource, Store, core, server } from './index.js';

const KEY = 'LfnKRnBLZzmIogDxU9-PVwrIYp7hnTsfOdx9c4gEpWM';
const WRITER = `did:ad:agent:${KEY}`;
const READER = 'did:ad:agent:reader-key-that-only-reads';
const DRIVE = 'https://atomicdata.dev/properties/drive';

function offlineStore(): Store {
  const store = new Store({
    serverUrl: 'https://atomic.example',
    connect: false,
  });
  // Nothing outside the store exists: a missing parent stays missing.
  store.injectFetch(async () => new Response('Not found', { status: 404 }));

  return store;
}

async function resourceIn(
  store: Store,
  subject: string,
  props: Record<string, string | string[]>,
): Promise<Resource> {
  const resource = new Resource(subject);

  for (const [prop, value] of Object.entries(props)) {
    await resource.set(prop, value, false);
  }

  store.addResource(resource);

  return resource;
}

describe('Resource.canWrite', () => {
  it('grants write to an agent listed under the other identifier spelling', async ({
    expect,
  }) => {
    const store = offlineStore();
    const drive = await resourceIn(store, 'atomic:resource:drive-a', {
      [core.properties.isA]: [server.classes.drive],
      [core.properties.write]: [`atomic:agent:${KEY}`],
    });

    expect((await drive.canWrite(WRITER))[0]).toBe(true);
  });

  it('grants write to a pre-DID URL grant for the same key', async ({
    expect,
  }) => {
    const store = offlineStore();
    const drive = await resourceIn(store, 'atomic:resource:drive-b', {
      [core.properties.isA]: [server.classes.drive],
      [core.properties.write]: [`https://old.example/agents/${KEY}`],
    });

    expect((await drive.canWrite(WRITER))[0]).toBe(true);
  });

  it('grants write to a pre-DID internal grant for the same key', async ({
    expect,
  }) => {
    const store = offlineStore();
    const drive = await resourceIn(store, 'atomic:resource:drive-internal', {
      [core.properties.isA]: [server.classes.drive],
      [core.properties.write]: [`internal:/agents/${KEY}`],
    });

    expect((await drive.canWrite(WRITER))[0]).toBe(true);
  });

  it('ignores a drive hint on the granted agent identifier, as the server does', async ({
    expect,
  }) => {
    const store = offlineStore();
    const drive = await resourceIn(store, 'atomic:resource:drive-hint', {
      [core.properties.isA]: [server.classes.drive],
      [core.properties.write]: [`atomic:agent:${KEY}?drive=atomic:resource:x`],
    });

    expect((await drive.canWrite(WRITER))[0]).toBe(true);
  });

  it('does not treat another base64 alphabet of the key as the same agent', async ({
    expect,
  }) => {
    // The server compares the key text as written (legacy paths keep their
    // standard-alphabet key), so a grant spelled `+/` does not authorize the
    // URL-safe `-_` spelling of the same bytes.
    const standardKey = 'U+/Hi4CrMCWr7O5waaKRPJ5Pq90T8ncocNkH0kYihCFM=';
    const urlSafeAgent =
      'did:ad:agent:U-_Hi4CrMCWr7O5waaKRPJ5Pq90T8ncocNkH0kYihCFM';
    const store = offlineStore();
    const drive = await resourceIn(store, 'atomic:resource:drive-alphabet', {
      [core.properties.isA]: [server.classes.drive],
      [core.properties.write]: [`https://old.example/agents/${standardKey}`],
    });

    expect((await drive.canWrite(urlSafeAgent))[0]).toBe(false);
    expect((await drive.canWrite(`did:ad:agent:${standardKey}`))[0]).toBe(true);
  });

  it('grants write through the drive stamp when the parent is not available', async ({
    expect,
  }) => {
    const store = offlineStore();
    const drive = await resourceIn(store, 'atomic:resource:drive-c', {
      [core.properties.isA]: [server.classes.drive],
      [core.properties.write]: [WRITER],
    });
    const article = await resourceIn(store, 'atomic:resource:article-c', {
      [core.properties.parent]: 'atomic:resource:folder-not-loaded',
      [DRIVE]: drive.subject,
    });

    expect((await article.canWrite(WRITER))[0]).toBe(true);
  });

  it('refuses write to a reader of the drive', async ({ expect }) => {
    const store = offlineStore();
    const drive = await resourceIn(store, 'atomic:resource:drive-d', {
      [core.properties.isA]: [server.classes.drive],
      [core.properties.write]: [WRITER],
      [core.properties.read]: [WRITER, READER],
    });
    const article = await resourceIn(store, 'atomic:resource:article-d', {
      [core.properties.parent]: drive.subject,
      [DRIVE]: drive.subject,
    });

    expect((await article.canWrite(READER))[0]).toBe(false);
  });

  it('refuses write when the parent chain is a cycle without a grant', async ({
    expect,
  }) => {
    const store = offlineStore();
    await resourceIn(store, 'atomic:resource:cycle-b', {
      [core.properties.parent]: 'atomic:resource:cycle-a',
    });
    const a = await resourceIn(store, 'atomic:resource:cycle-a', {
      [core.properties.parent]: 'atomic:resource:cycle-b',
    });

    expect((await a.canWrite(WRITER))[0]).toBe(false);
  });
});
