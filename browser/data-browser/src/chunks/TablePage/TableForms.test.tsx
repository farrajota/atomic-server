// @vitest-environment jsdom
import { act, cleanup, render, screen, waitFor } from '@testing-library/react';
import type * as ReactExports from '@tomic/react';
import { core, forms, Resource, Store } from '@tomic/lib';
import type {
  ClientDbQueryOpts,
  ClientDbQueryResult,
  ClientDbWorker,
} from '@tomic/lib';
import { createElement, type ReactNode } from 'react';
import { afterEach, expect, it, vi } from 'vitest';
import { StoreContext } from '../../../../react/src/hooks';
import { TableForms } from './TableForms';

vi.mock('@tomic/react', async importOriginal => {
  const actual = await importOriginal<typeof ReactExports>();
  const hooks = await import('../../../../react/src/hooks');
  const collection = await import('../../../../react/src/useCollection');
  const member = await import('../../../../react/src/useMemberFromCollection');

  return {
    ...actual,
    useCollection: collection.useCollection,
    useMemberFromCollection: member.useMemberFromCollection,
    useString: hooks.useString,
  };
});

vi.mock('@views/ResourceInline/ResourceInline', () => ({
  ResourceInline: ({ subject }: { subject: string }) =>
    createElement('span', { 'data-testid': 'form-link' }, subject),
}));

const DRIVE_PROPERTY = 'https://atomicdata.dev/properties/drive';
const TABLE = 'atomic:resource:table';
const FORM = 'atomic:resource:form';
const SECOND_FORM = 'atomic:resource:second-form';
const PRIVATE_DRIVE = 'did:ad:drive:private';
const SECOND_PRIVATE_DRIVE = 'did:ad:drive:second-private';
const ACTIVE_DRIVE = 'did:ad:drive:factory';

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function formsClientDb(calls: ClientDbQueryOpts[]): ClientDbWorker {
  const formsByDrive: Record<string, string> = {
    [PRIVATE_DRIVE]: FORM,
    [SECOND_PRIVATE_DRIVE]: SECOND_FORM,
  };
  const allForms = Object.values(formsByDrive);

  return {
    isReady: true,
    isInitialized: true,
    waitForInit: async () => true,
    waitForReady: async () => true,
    getResourcesWithSnapshots: async (subjects: string[]) =>
      subjects.map(subject => ({
        jsonAd: allForms.includes(subject)
          ? JSON.stringify({
              '@id': subject,
              [core.properties.parent]: TABLE,
              [core.properties.isA]: [forms.classes.form],
            })
          : null,
        snapshot: null,
      })),
    query: async (options: ClientDbQueryOpts): Promise<ClientDbQueryResult> => {
      calls.push(options);
      const member = formsByDrive[options.drive ?? ''];
      const matches =
        options.property === core.properties.parent &&
        options.value === TABLE &&
        options.filters?.some(
          filter =>
            filter.property === core.properties.isA &&
            filter.value === forms.classes.form,
        );
      if (!member || !matches) return { subjects: [], resources: [], count: 0 };

      return {
        subjects: [member],
        resources: [
          JSON.stringify({
            '@id': member,
            [core.properties.parent]: TABLE,
            [core.properties.isA]: [forms.classes.form],
          }),
        ],
        count: 1,
      };
    },
  } as unknown as ClientDbWorker;
}

it('lists and refreshes table-owned forms while another drive is active', async () => {
  const store = new Store({
    serverUrl: 'http://localhost:9883',
    connect: false,
  });
  store.setDrive(ACTIVE_DRIVE);
  store.finishDriveSync(ACTIVE_DRIVE, 0, Date.now());
  store.finishDriveSync(PRIVATE_DRIVE, 1, Date.now());
  store.finishDriveSync(SECOND_PRIVATE_DRIVE, 1, Date.now());
  const calls: ClientDbQueryOpts[] = [];
  store.setClientDb(formsClientDb(calls));

  const table = new Resource(TABLE);
  table.setStore(store);
  await table.set(DRIVE_PROPERTY, PRIVATE_DRIVE, false);
  const wrapper = ({ children }: { children: ReactNode }) =>
    createElement(StoreContext.Provider, { value: store }, children);

  const result = render(createElement(TableForms, { table }), { wrapper });

  await waitFor(() =>
    expect(screen.getByTestId('form-link').textContent).toBe(FORM),
  );
  expect(calls).toContainEqual(
    expect.objectContaining({
      property: core.properties.parent,
      value: TABLE,
      drive: PRIVATE_DRIVE,
      filters: [{ property: core.properties.isA, value: forms.classes.form }],
    }),
  );

  await act(async () => {
    await table.set(DRIVE_PROPERTY, SECOND_PRIVATE_DRIVE, false);
  });
  await waitFor(() =>
    expect(screen.getByTestId('form-link').textContent).toBe(SECOND_FORM),
  );
  expect(calls).toContainEqual(
    expect.objectContaining({
      property: core.properties.parent,
      value: TABLE,
      drive: SECOND_PRIVATE_DRIVE,
      filters: [{ property: core.properties.isA, value: forms.classes.form }],
    }),
  );

  result.unmount();
  render(createElement(TableForms, { table }), { wrapper });
  await waitFor(() =>
    expect(screen.getByTestId('form-link').textContent).toBe(SECOND_FORM),
  );
  expect(store.getDrive()).toBe(ACTIVE_DRIVE);
});
