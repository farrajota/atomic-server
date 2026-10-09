import { describe, it } from 'vitest';
import { Resource, core, server } from './index.js';
import { testStore } from './test-store.js';

const DRIVE = 'https://atomicdata.dev/properties/drive';
const ACTIVE_DRIVE = 'atomic:resource:active-drive-not-the-parents';

/** Driveless resources in the store, each the parent of the previous one. */
async function addChain(
  store: Awaited<ReturnType<typeof testStore>>['store'],
  name: string,
  length: number,
): Promise<string[]> {
  const subjects = Array.from(
    { length },
    (_, i) => `atomic:resource:${name}-${i}`,
  );

  for (const [i, subject] of subjects.entries()) {
    const resource = new Resource(subject);

    if (subjects[i + 1]) {
      await resource.set(core.properties.parent, subjects[i + 1], false);
    }

    store.addResource(resource);
  }

  return subjects;
}

async function markDrive(
  store: Awaited<ReturnType<typeof testStore>>['store'],
  subject: string,
) {
  const resource = store.resources.get(subject)!;
  await resource.set(core.properties.isA, [server.classes.drive], false);
}

/** Lets `uploadFiles` run without the WASM client database. */
function stubClientDb(store: Awaited<ReturnType<typeof testStore>>['store']) {
  (store as unknown as { clientDb: unknown }).clientDb = {
    isReady: true,
    waitForInit: async () => true,
    blake3Hash: async () => new Uint8Array(32),
    putBlob: async () => undefined,
    flush: async () => undefined,
    putResourceWithSnapshot: async () => undefined,
  };
}

describe('drive lineage of new resources', () => {
  it('stamps no drive, not even the active one, when the parent cannot be loaded', async ({
    expect,
  }) => {
    const { store } = await testStore();
    store.setDrive(ACTIVE_DRIVE);
    const unreachableParent = 'atomic:resource:parent-that-fails-to-load';
    store.injectFetch(async () => new Response('boom', { status: 500 }));

    const child = await store.newResource({
      parent: unreachableParent,
      isA: 'https://atomicdata.dev/classes/Article',
    });

    // The genesis commit is signed inside `newResource`, so this is the
    // value it carries.
    expect(child.get(core.properties.parent)).toBe(unreachableParent);
    expect(child.get(DRIVE)).toBeUndefined();
    expect(Object.values(child.getPropVals())).not.toContain(ACTIVE_DRIVE);
  });

  it('stamps no drive when the parent chain is a cycle', async ({ expect }) => {
    const { store } = await testStore();
    store.setDrive(ACTIVE_DRIVE);
    const a = new Resource('atomic:resource:cycle-a');
    const b = new Resource('atomic:resource:cycle-b');
    await a.set(core.properties.parent, b.subject, false);
    await b.set(core.properties.parent, a.subject, false);
    store.addResource(a);
    store.addResource(b);

    const child = await store.newResource({ parent: a.subject });

    expect(child.get(DRIVE)).toBeUndefined();
  });

  it('finds a drive 32 ancestors up', async ({ expect }) => {
    const { store } = await testStore();
    const chain = await addChain(store, 'within-bound', 32);
    await markDrive(store, chain[31]);

    const child = await store.newResource({ parent: chain[0] });

    expect(child.get(DRIVE)).toBe(chain[31]);
  });

  it('stops looking after 32 ancestors instead of walking on', async ({
    expect,
  }) => {
    const { store } = await testStore();
    store.setDrive(ACTIVE_DRIVE);
    const chain = await addChain(store, 'past-bound', 33);
    await markDrive(store, chain[32]);

    const child = await store.newResource({ parent: chain[0] });

    expect(child.get(DRIVE)).toBeUndefined();
  });

  it('still stamps the active drive on a top-level resource without a parent', async ({
    expect,
  }) => {
    const { store } = await testStore();
    store.setDrive(ACTIVE_DRIVE);

    const resource = await store.newResource({
      noParent: true,
      isA: 'https://atomicdata.dev/classes/Article',
    });

    expect(resource.get(DRIVE)).toBe(ACTIVE_DRIVE);
  });

  it('stamps an upload with the drive from its parent lineage, never the parent itself', async ({
    expect,
  }) => {
    const { store } = await testStore();
    store.setDrive(ACTIVE_DRIVE);
    stubClientDb(store);
    const chain = await addChain(store, 'upload-lineage', 3);
    await markDrive(store, chain[2]);

    const [subject] = await store.uploadFiles(
      [new File(['hello'], 'hello.txt')],
      chain[0],
    );

    expect(store.resources.get(subject)!.get(DRIVE)).toBe(chain[2]);
  });

  it('stamps no drive on an upload whose parent cannot be loaded', async ({
    expect,
  }) => {
    const { store } = await testStore();
    store.setDrive(ACTIVE_DRIVE);
    stubClientDb(store);
    const unreachableParent = 'atomic:resource:upload-parent-that-fails';
    store.injectFetch(async () => new Response('boom', { status: 500 }));

    const [subject] = await store.uploadFiles(
      [new File(['hello'], 'hello.txt')],
      unreachableParent,
    );
    const file = store.resources.get(subject)!;

    expect(file.get(core.properties.parent)).toBe(unreachableParent);
    expect(file.get(DRIVE)).toBeUndefined();
    expect(Object.values(file.getPropVals())).not.toContain(ACTIVE_DRIVE);
  });
});
