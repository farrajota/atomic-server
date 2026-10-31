// @vitest-environment jsdom
import { cleanup, renderHook, waitFor } from '@testing-library/react';
import type * as ReactExports from '@tomic/react';
import { core, dataBrowser, Resource, Store } from '@tomic/lib';
import type {
  ClientDbQueryOpts,
  ClientDbQueryResult,
  ClientDbWorker,
} from '@tomic/lib';
import { createElement, type ReactNode } from 'react';
import { afterEach, expect, it, vi } from 'vitest';
import { StoreContext } from '../../../../react/src/hooks';
import { useTableData } from './useTableData';

// This mock crosses the package boundary deliberately: keep its unrelated exports,
// but bind collection and Store hooks to the source modules named by this test.
// Vitest hoists mock factories before static imports, so these source modules
// must be loaded inside the factory.
vi.mock('@tomic/react', async importOriginal => {
  const actual = await importOriginal<typeof ReactExports>();
  const hooks = await import('../../../../react/src/hooks');
  const collection = await import('../../../../react/src/useCollection');

  return {
    ...actual,
    useArray: hooks.useArray,
    useBoolean: hooks.useBoolean,
    useCanWrite: hooks.useCanWrite,
    useCollection: collection.useCollection,
    useResource: hooks.useResource,
    useStore: hooks.useStore,
    useString: hooks.useString,
    useSubject: hooks.useSubject,
    useValue: hooks.useValue,
  };
});

vi.mock('../../routes/ShowRoute', () => ({
  ShowRoute: {
    useNavigate: () => vi.fn(),
    useSearch: () => undefined,
  },
}));

const DRIVE_PROPERTY = 'https://atomicdata.dev/properties/drive';
const TABLE = 'atomic:resource:table';
const CLASS = dataBrowser.classes.article;
const PRIVATE_DRIVE = 'did:ad:drive:private';
const ACTIVE_DRIVE = 'did:ad:drive:factory';
const ROW = 'atomic:resource:article';

afterEach(() => {
  cleanup();
});

function tableClientDb(calls: ClientDbQueryOpts[]): ClientDbWorker {
  return {
    isReady: true,
    isInitialized: true,
    waitForInit: async () => true,
    waitForReady: async () => true,
    getResourcesWithSnapshots: async (subjects: string[]) =>
      subjects.map(subject => ({
        jsonAd:
          subject === ROW
            ? JSON.stringify({
                '@id': ROW,
                [core.properties.parent]: TABLE,
                [core.properties.isA]: [CLASS],
              })
            : null,
        snapshot: null,
      })),
    query: async (options: ClientDbQueryOpts): Promise<ClientDbQueryResult> => {
      calls.push(options);
      const classFilter = options.filters?.some(
        filter =>
          filter.property === core.properties.isA && filter.value === CLASS,
      );
      const matches =
        options.drive === PRIVATE_DRIVE &&
        options.property === core.properties.parent &&
        options.value === TABLE &&
        classFilter;

      return matches
        ? {
            subjects: [ROW],
            resources: [
              JSON.stringify({
                '@id': ROW,
                [core.properties.parent]: TABLE,
                [core.properties.isA]: [CLASS],
              }),
            ],
            count: 1,
          }
        : { subjects: [], resources: [], count: 0 };
    },
  } as unknown as ClientDbWorker;
}

it('shows the table drive row without changing the active workspace', async () => {
  const store = new Store({
    serverUrl: 'http://localhost:9883',
    connect: false,
  });
  store.setDrive(ACTIVE_DRIVE);
  store.finishDriveSync(ACTIVE_DRIVE, 0, Date.now());
  store.finishDriveSync(PRIVATE_DRIVE, 1, Date.now());
  const calls: ClientDbQueryOpts[] = [];
  store.setClientDb(tableClientDb(calls));
  const table = new Resource(TABLE);
  table.setStore(store);
  await table.set(core.properties.isA, [dataBrowser.classes.table], false);
  await table.set(core.properties.classtype, CLASS, false);
  await table.set(DRIVE_PROPERTY, PRIVATE_DRIVE, false);
  const wrapper = ({ children }: { children: ReactNode }) =>
    createElement(StoreContext.Provider, { value: store }, children);

  const result = renderHook(() => useTableData(table), { wrapper });
  await waitFor(async () => {
    expect(result.result.current.ready).toBe(true);
    await expect(
      result.result.current.collection.getAllMembers(),
    ).resolves.toEqual([ROW]);
  });

  expect(calls).toContainEqual(
    expect.objectContaining({
      property: core.properties.parent,
      value: TABLE,
      drive: PRIVATE_DRIVE,
      filters: [{ property: core.properties.isA, value: CLASS }],
    }),
  );
  expect(store.getDrive()).toBe(ACTIVE_DRIVE);
});
