import { describe, expect, it, vi } from 'vitest';
import type { Store } from '@tomic/lib';
import {
  deviceHasDriveData,
  driveHasServer,
  findAccessibleDrives,
} from './driveData';
import { isOriginWithoutNode } from './originNode';

const collectionState = vi.hoisted(() => ({
  getAllMembers: vi.fn<() => Promise<string[]>>(),
  constructorArgs: undefined as unknown[] | undefined,
}));

vi.mock('@tomic/lib', () => ({
  Collection: vi.fn(function MockCollection(...args: unknown[]) {
    collectionState.constructorArgs = args;

    return collectionState;
  }),
  core: { properties: { isA: 'https://atomicdata.dev/properties/isA' } },
  server: { classes: { drive: 'https://atomicdata.dev/classes/Drive' } },
}));

vi.mock('./originNode', () => ({ isOriginWithoutNode: vi.fn() }));

describe('refreshing drive availability', () => {
  it('rechecks an earlier failure after data arrives instead of retaining the miss', async () => {
    vi.mocked(isOriginWithoutNode).mockReturnValue(true);
    let error: Error | undefined = new Error('Not found');
    const store = {
      getServerUrl: () => 'https://app.example',
      isLocalOnlyDrive: () => true,
      getResource: vi.fn(async () => ({ error })),
      getResourceLoading: () => ({ error }),
      reloadResource: vi.fn(async () => {
        error = undefined;
      }),
    };
    const typedStore = store as unknown as Store;
    expect(await deviceHasDriveData(typedStore, 'did:ad:drive')).toBe(false);
    expect(
      await deviceHasDriveData(typedStore, 'did:ad:drive', { refresh: true }),
    ).toBe(true);
  });

  it('does not report an errored resource as available just because the read resolved', async () => {
    const store = {
      getResource: vi
        .fn()
        .mockResolvedValue({ error: new Error('Unauthorized') }),
    };
    expect(
      await deviceHasDriveData(store as unknown as Store, 'did:ad:drive'),
    ).toBe(false);
  });

  it('does not report availability when local storage rejects the read', async () => {
    const store = {
      getResource: vi.fn().mockRejectedValue(new Error('Storage unavailable')),
    };
    expect(
      await deviceHasDriveData(store as unknown as Store, 'did:ad:drive'),
    ).toBe(false);
  });
  it.each([
    [true, false, true],
    [false, true, true],
    [false, false, false],
  ])(
    'nodeless=%s localOnly=%s reloadsLocal=%s',
    async (nodeless, localOnly, local) => {
      vi.mocked(isOriginWithoutNode).mockReturnValue(nodeless);
      const store = {
        getServerUrl: () => 'https://app.example',
        isLocalOnlyDrive: () => localOnly,
        reloadResource: vi.fn().mockResolvedValue(undefined),
        fetchResourceFromServer: vi.fn().mockResolvedValue(undefined),
        getResourceLoading: () => ({ error: undefined }),
      };
      expect(
        await deviceHasDriveData(store as unknown as Store, 'did:ad:drive', {
          refresh: true,
        }),
      ).toBe(true);
      expect(store.reloadResource).toHaveBeenCalledTimes(local ? 1 : 0);
      expect(store.fetchResourceFromServer).toHaveBeenCalledTimes(
        local ? 0 : 1,
      );
    },
  );
});

describe('driveHasServer', () => {
  it.each([
    [false, false, true],
    [true, false, false],
    [false, true, false],
    [true, true, false],
  ])('nodeless=%s localOnly=%s hasServer=%s', (nodeless, localOnly, has) => {
    vi.mocked(isOriginWithoutNode).mockReturnValue(nodeless);
    const store = {
      getServerUrl: () => 'https://app.example',
      isLocalOnlyDrive: () => localOnly,
    };
    expect(driveHasServer(store, 'did:ad:drive')).toBe(has);
  });
});

describe('discovering accessible drives', () => {
  it('returns sorted existing Drives only after the active Agent can read them', async () => {
    collectionState.getAllMembers.mockResolvedValue([
      'did:ad:shared-z',
      'did:ad:private-to-another-agent',
      'did:ad:shared-a',
    ]);
    vi.mocked(isOriginWithoutNode).mockReturnValue(false);
    const signedInAgent = { subject: 'did:ad:agent:service' };
    const store = {
      getServerUrl: () => 'https://node.example',
      getAgent: () => signedInAgent,
      isLocalOnlyDrive: () => false,
      fetchResourceFromServer: vi.fn(),
      getResourceLoading: (subject: string) => ({
        error:
          subject === 'did:ad:private-to-another-agent'
            ? new Error('Unauthorized')
            : undefined,
      }),
    };

    expect(await findAccessibleDrives(store as unknown as Store)).toEqual([
      'did:ad:shared-a',
      'did:ad:shared-z',
    ]);
    expect(collectionState.constructorArgs).toEqual([
      store,
      'https://node.example',
      {
        property: 'https://atomicdata.dev/properties/isA',
        value: 'https://atomicdata.dev/classes/Drive',
        page_size: '100',
        include_nested: false,
        drive: undefined,
      },
      false,
      { serverOnly: true },
    ]);
  });
  it('discards query results if the signed-in Agent changes during read checks', async () => {
    collectionState.getAllMembers.mockResolvedValue(['did:ad:shared']);
    vi.mocked(isOriginWithoutNode).mockReturnValue(false);
    let agent = { subject: 'did:ad:agent:first' };
    const store = {
      getServerUrl: () => 'https://node.example',
      getAgent: () => agent,
      isLocalOnlyDrive: () => false,
      fetchResourceFromServer: vi.fn(async () => {
        agent = { subject: 'did:ad:agent:second' };
      }),
      getResourceLoading: () => ({ error: undefined }),
    };

    expect(await findAccessibleDrives(store as unknown as Store)).toEqual([]);
  });
});
