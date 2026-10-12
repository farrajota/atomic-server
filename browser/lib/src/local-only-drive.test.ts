import { describe, it, vi } from 'vitest';
import {
  Resource,
  commits,
  core,
  server,
  isAtomicIdentifier,
  isCommitSubject,
} from './index.js';
import { testStore } from './test-store.js';
import type { Store } from './store.js';

function attachDurableSnapshotDb(store: Store) {
  const snapshots = new Map<string, { jsonAd: string; snapshot: Uint8Array }>();
  store.setClientDb({
    isOpen: true,
    isReady: true,
    isInitialized: true,
    whenOpen: async () => true,
    waitForInit: async () => true,
    waitForReady: async () => true,
    getResourceWithSnapshot: async (subject: string) =>
      snapshots.get(subject) ?? { jsonAd: null, snapshot: undefined },
    getResourcesWithSnapshots: async (subjects: string[]) =>
      subjects.map(
        subject =>
          snapshots.get(subject) ?? { jsonAd: null, snapshot: undefined },
      ),
    unsupportedEnvironment: false,
    putResourceWithSnapshot: async (
      subject: string,
      jsonAd: string,
      snapshot?: Uint8Array,
    ) => {
      if (!snapshot) throw new Error('Expected a durable resource snapshot');
      snapshots.set(subject, { jsonAd, snapshot: snapshot.slice() });
    },
  } as never);

  return snapshots;
}

/**
 * Local-only drives (e.g. the demo workspace): resources save and
 * stamp lastCommit entirely client-side — nothing may ever be POSTed
 * or enrolled in the outbox.
 */
describe('Local-only drives', () => {
  it('waits for a readable root before asking for the server inventory', async ({
    expect,
  }) => {
    const { store } = await testStore();
    const drive = 'atomic:uploading-drive';
    const ws = {
      readyState: WebSocket.OPEN,
      resyncDrive: vi.fn(async () => {}),
      fetch: vi
        .fn()
        .mockRejectedValueOnce(new Error('Root not found yet'))
        .mockResolvedValue({}),
      driveInventory: vi.fn(async () => [{ subject: drive, vv: { peer: 1 } }]),
    };
    vi.spyOn(store, 'getDefaultWebSocket').mockReturnValue(ws as never);
    vi.spyOn(store, 'getClientDb').mockReturnValue({
      flush: vi.fn(async () => {}),
      getVersionVectorsForDrive: vi.fn(async () => ({ [drive]: { peer: 1 } })),
    } as never);
    vi.spyOn(store, 'subscribeWebSocket').mockImplementation(() => {});
    vi.useFakeTimers();

    try {
      const upload = store.syncDriveToServerAndVerify(drive);
      await vi.waitFor(() => expect(ws.fetch).toHaveBeenCalledTimes(1));
      expect(ws.driveInventory).not.toHaveBeenCalled();
      await vi.advanceTimersByTimeAsync(2000);
      await upload;
      expect(ws.driveInventory).toHaveBeenCalledExactlyOnceWith(drive, '');
    } finally {
      vi.useRealTimers();
      vi.restoreAllMocks();
    }
  });
  it('saves the drive without POSTing or enrolling the outbox', async ({
    expect,
  }) => {
    const { store, posted } = await testStore();
    const snapshots = attachDurableSnapshotDb(store);

    const drive = await store.newResource({
      isA: server.classes.drive,
      noParent: true,
      propVals: { [core.properties.name]: 'Demo team' },
    });
    store.registerLocalOnlyDrive(drive.subject);

    const result = await drive.save();

    expect(result).toBe('persisted');
    expect(posted).toHaveLength(0);
    expect(store.outbox.size).toBe(0);
    const persisted = snapshots.get(drive.subject)!;
    expect(JSON.parse(persisted.jsonAd)[core.properties.name]).toBe(
      'Demo team',
    );
    expect(persisted.snapshot).toBeInstanceOf(Uint8Array);
    const reloaded = new Resource(drive.subject);
    reloaded.importLoroUpdate(persisted.snapshot);
    expect(reloaded.get(core.properties.name)).toBe('Demo team');
    expect(isAtomicIdentifier(drive.subject)).toBe(true);

    const lastCommit = String(drive.get(commits.properties.lastCommit));
    expect(isCommitSubject(lastCommit)).toBe(true);
    expect(store.resources.has(lastCommit)).toBe(false);
  });
  it('creates and saves a parentless local-only drive without a server URL', async ({
    expect,
  }) => {
    const { store, posted } = await testStore({ serverUrl: undefined });
    const snapshots = attachDurableSnapshotDb(store);

    const drive = await store.createDrive('Offline workspace', {
      localOnly: true,
    });
    const child = await store.newResource({
      parent: drive.subject,
      propVals: { [core.properties.name]: 'Welcome' },
    });

    expect(isAtomicIdentifier(drive.subject)).toBe(true);
    expect(drive.get(core.properties.parent)).toBeUndefined();
    expect(drive.get('https://atomicdata.dev/properties/genesis')).toBeTruthy();
    expect(child.get(core.properties.parent)).toBe(drive.subject);
    expect(child.get('https://atomicdata.dev/properties/drive')).toBe(
      drive.subject,
    );
    expect(store.isLocalOnlySubject(child.subject)).toBe(true);

    await child.save();

    for (const resource of [drive, child]) {
      const persisted = snapshots.get(resource.subject);
      expect(persisted).toBeDefined();
      expect(persisted!.snapshot).toBeInstanceOf(Uint8Array);
      const reloaded = new Resource(resource.subject);
      reloaded.importLoroUpdate(persisted!.snapshot);
      expect(reloaded.get(core.properties.name)).toBe(
        resource.get(core.properties.name),
      );
    }

    expect(posted).toHaveLength(0);
    expect(store.outbox.size).toBe(0);
  });

  it('retains a local-only save after its required snapshot is unavailable', async ({
    expect,
  }) => {
    const { store } = await testStore();
    const drive = await store.newResource({
      isA: server.classes.drive,
      noParent: true,
      propVals: { [core.properties.name]: 'Local-only drive' },
    });
    store.registerLocalOnlyDrive(drive.subject);
    const saved = vi.spyOn(store, 'notifyResourceSaved');

    await expect(drive.save()).rejects.toThrow(/local database|storage/i);
    expect(saved).not.toHaveBeenCalled();
    expect(store.outbox.size).toBe(0);

    const snapshots = attachDurableSnapshotDb(store);
    await expect(drive.save()).resolves.toBe('persisted');
    expect(saved).toHaveBeenCalledOnce();
    expect(snapshots.get(drive.subject)?.snapshot).toBeInstanceOf(Uint8Array);
    expect(store.outbox.size).toBe(0);
  });

  it('resolves children as local-only and stamps lastCommit locally', async ({
    expect,
  }) => {
    const { store, posted } = await testStore();
    attachDurableSnapshotDb(store);

    const drive = await store.newResource({
      isA: server.classes.drive,
      noParent: true,
      propVals: { [core.properties.name]: 'Demo team' },
    });
    store.registerLocalOnlyDrive(drive.subject);
    await drive.save();

    const doc = await store.newResource({
      parent: drive.subject,
      propVals: { [core.properties.name]: 'Welcome' },
    });

    expect(store.isLocalOnlySubject(doc.subject)).toBe(true);

    await doc.save();
    const first = String(doc.get(commits.properties.lastCommit));
    expect(isCommitSubject(first)).toBe(true);

    await doc.set(core.properties.name, 'Welcome aboard');
    await doc.save();
    const second = String(doc.get(commits.properties.lastCommit));

    expect(second).not.toBe(first);

    expect(posted).toHaveLength(0);
    expect(store.outbox.size).toBe(0);
  });

  it('leaves other drives untouched', async ({ expect }) => {
    const { store, posted } = await testStore();
    attachDurableSnapshotDb(store);

    const localDrive = await store.newResource({
      isA: server.classes.drive,
      noParent: true,
      propVals: { [core.properties.name]: 'Demo team' },
    });
    store.registerLocalOnlyDrive(localDrive.subject);
    await localDrive.save();

    // A resource outside the local-only drive still drains normally.
    const synced = await store.newResource({
      propVals: { [core.properties.name]: 'Synced' },
    });

    expect(store.isLocalOnlySubject(synced.subject)).toBe(false);

    await synced.save();

    expect(posted.length).toBeGreaterThan(0);
    expect(posted.every(commit => commit.subject !== localDrive.subject)).toBe(
      true,
    );
  });
});
