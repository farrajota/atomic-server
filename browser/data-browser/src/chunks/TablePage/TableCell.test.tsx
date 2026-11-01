// @vitest-environment jsdom
// @wc-ignore-file
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { StrictMode, useCallback, useMemo, useState } from 'react';
import userEvent from '@testing-library/user-event';
import { beforeAll, afterEach, describe, expect, it, vi } from 'vitest';
import {
  act,
  cleanup,
  configure,
  fireEvent,
  getConfig,
  render,
  screen,
  waitFor,
} from '@testing-library/react';
import { ThemeProvider } from 'styled-components';
import {
  core,
  Datatype,
  type JSONValue,
  LoroLoader,
  Store,
  StoreContext,
  type Property,
  type Resource,
} from '@tomic/react';
import { FancyTable } from '@chunks/TableEditor';
import { buildTheme } from '../../styling';
import { QuickFilterField } from './QuickFilterField';
import { TableRow } from './TableRow';
import { TableCell } from './TableCell';
import { subjectListCollection, useQuickFilter } from './useQuickFilter';

vi.mock('@chunks/TableEditor/DndWrapper', () => ({
  DndWrapper: ({ children }: { children: React.ReactNode }) => children,
}));
vi.mock('@chunks/TableEditor/TableHeader', () => ({
  TableHeader: () => <div role='row' />,
}));
// jsdom has no layout, so the grid's height probe would hand react-window a
// zero-height viewport and no row would render.
vi.mock('react-virtualized-auto-sizer', () => ({
  AutoSizer: ({
    renderProp,
  }: {
    renderProp: (props: { height: number }) => React.ReactNode;
  }) => renderProp({ height: 400 }),
}));
vi.mock('./helpers/useColumnLabel', () => ({ useColumnLabel: () => 'Day' }));

const DAY = 'https://example.com/properties/day';
const STORED = '2026-09-25';

const dayProperty: Property = {
  subject: DAY,
  datatype: Datatype.DATE,
  shortname: 'day',
  description: '',
};

const SEARCH_TITLE = '_new:search-title';
const SEARCH_DESCRIPTION = '_new:search-description';
const SEARCH_ROW = 'https://example.com/rows/search-regression';
const SAVED_DESCRIPTION = 'saved description';

const titleProperty: Property = {
  subject: SEARCH_TITLE,
  datatype: Datatype.STRING,
  shortname: 'title',
  description: '',
};

const descriptionProperty: Property = {
  subject: SEARCH_DESCRIPTION,
  datatype: Datatype.STRING,
  shortname: 'description',
  description: '',
};

const searchColumns = [
  { property: titleProperty },
  { property: descriptionProperty },
];

const searchTableColumns = searchColumns.map(({ property }) => ({
  key: property.subject,
  property,
}));

function QuickFilterTableHarness({ rowSubject }: { rowSubject: string }) {
  const [query, setQuery] = useState('');
  const [allRows] = useState(() => [rowSubject]);
  const collection = useMemo(() => subjectListCollection(allRows), [allRows]);
  const quickFilter = useQuickFilter(collection, searchColumns, query);
  const itemCount = quickFilter.active
    ? quickFilter.matches.length
    : allRows.length;
  const quickFilterQuery = query.trim();
  const renderRow = useCallback(
    ({ index }: { index: number }) => (
      <TableRow
        collection={quickFilter.collection}
        index={index}
        columns={searchTableColumns}
        subject={quickFilter.active ? quickFilter.matches[index] : undefined}
      />
    ),
    [quickFilter.active, quickFilter.collection, quickFilter.matches],
  );
  const itemKey = useCallback(
    (index: number) =>
      quickFilter.active
        ? `match-${quickFilter.matches[index] ?? index}`
        : `member-${index}`,
    [quickFilter.active, quickFilter.matches],
  );

  return (
    <>
      <QuickFilterField value={query} onChange={setQuery} />
      {quickFilter.active &&
        !quickFilter.loading &&
        quickFilter.matches.length === 0 && (
          <p role='status'>{`No rows show “${quickFilterQuery}”.`}</p>
        )}
      <FancyTable
        columns={searchTableColumns}
        itemCount={itemCount}
        itemKey={itemKey}
        columnToKey={column => column.key}
        labelledBy='table-title'
        HeadingComponent={() => <></>}
        NewColumnButtonComponent={() => null}
      >
        {renderRow}
      </FancyTable>
    </>
  );
}

beforeAll(async () => {
  // The app's vite config sends every `loro-crdt` import to the `web` build,
  // which fetches its WASM over HTTP. Hand it the file from disk instead;
  // the loader's own init then finds it initialized.
  const wasm = await readFile(
    createRequire(import.meta.url).resolve('loro-crdt/web/loro_wasm_bg.wasm'),
  );
  const web = (await import('loro-crdt/web')) as unknown as {
    default: (opts: { module_or_path: Uint8Array }) => Promise<unknown>;
  };
  await web.default({ module_or_path: wasm });
  await LoroLoader.initializeLoro();

  class ResizeObserverStub {
    public observe() {}
    public unobserve() {}
    public disconnect() {}
  }

  globalThis.ResizeObserver =
    ResizeObserverStub as unknown as typeof ResizeObserver;
});

afterEach(cleanup);

async function renderDateCell(): Promise<Resource> {
  return renderCell(dayProperty, STORED);
}

async function renderCell(
  property: Property,
  stored: JSONValue,
): Promise<Resource> {
  const store = new Store({ serverUrl: 'https://example.com', connect: false });
  // A `_new:` row stays local, so nothing here reaches for a server.
  const row = await store.newResource({
    subject: '_new:day-row',
    noParent: true,
  });
  await row.set(property.subject, stored, false);

  // StrictMode as the app runs it: an editor that stores on unmount must not
  // store when React only re-runs its effects.
  render(
    <StrictMode>
      <StoreContext value={store}>
        <ThemeProvider theme={buildTheme(false, '#1b50d8')}>
          <FancyTable
            columns={['title', 'day']}
            itemCount={1}
            columnToKey={String}
            labelledBy='table-title'
            HeadingComponent={() => <></>}
            NewColumnButtonComponent={() => null}
          >
            {() => (
              <TableCell
                rowIndex={0}
                columnIndex={1}
                subject={row.subject}
                property={property}
              />
            )}
          </FancyTable>
        </ThemeProvider>
      </StoreContext>
    </StrictMode>,
  );

  return row;
}

function editedCell(): HTMLElement {
  const cell = document.querySelector(
    '[aria-rowindex="2"] > [aria-colindex="2"]',
  );

  if (!cell) {
    throw new Error('No cell rendered');
  }

  return cell as HTMLElement;
}

async function renderSearchableTable() {
  const store = new Store({ serverUrl: 'https://example.com', connect: false });

  for (const [property, label] of [
    [titleProperty, 'Title'],
    [descriptionProperty, 'Description'],
  ] as const) {
    const propertyResource = await store.newResource({
      subject: property.subject,
      noParent: true,
    });
    await propertyResource.set(core.properties.name, label, false);
  }

  const row = await store.newResource({ subject: SEARCH_ROW, noParent: true });
  await row.set(SEARCH_TITLE, 'needle', false);
  await row.set(SEARCH_DESCRIPTION, SAVED_DESCRIPTION, false);
  const save = vi
    .spyOn(row, 'save')
    .mockImplementation(async () => undefined as never);

  render(
    <StrictMode>
      <StoreContext value={store}>
        <ThemeProvider theme={buildTheme(false, '#1b50d8')}>
          <QuickFilterTableHarness rowSubject={row.subject} />
        </ThemeProvider>
      </StoreContext>
    </StrictMode>,
  );

  await waitFor(() =>
    expect(searchDescriptionCell().textContent).toContain(SAVED_DESCRIPTION),
  );

  return { row, save };
}

function searchDescriptionCell(): HTMLElement {
  const cell = document.querySelector(
    '[aria-rowindex="2"] > [aria-colindex="3"]',
  );

  if (!cell) {
    throw new Error('No Description cell rendered');
  }

  return cell as HTMLElement;
}

/** Select the cell (not editing yet) and type one character on it. */
async function typeOnSelectedCell(key: string) {
  fireEvent.mouseDown(editedCell());
  fireEvent.click(editedCell());
  expect(screen.queryByRole('textbox')).toBeNull();

  await act(async () => {
    fireEvent.keyDown(editedCell(), { key });
  });
  await settle();
}

/** Lets the cell's async writes (`resource.set`, the debounced save) land. */
async function settle() {
  await act(() => new Promise(resolve => setTimeout(resolve, 300)));
}

describe('quick-filter keyboard focus', () => {
  it('keeps native outside focus and both row values after deferred Name editor exit', async () => {
    const user = userEvent.setup();
    const { row, save } = await renderSearchableTable();
    const search = screen.getByRole('textbox', {
      name: 'Find rows containing text',
    }) as HTMLInputElement;

    const clickSearch = async () => {
      // Let this act scope own the complete user click. RTL's normal asyncWrapper
      // clears the act environment, which would overlap this outer act.
      const originalAsyncWrapper = getConfig().asyncWrapper;
      configure({ asyncWrapper: callback => callback() });

      try {
        await act(async () => {
          await user.click(search);
          expect(document.activeElement).toBe(search);
        });
      } finally {
        configure({ asyncWrapper: originalAsyncWrapper });
      }
    };

    await user.click(editedCell());
    await user.keyboard('{Enter}');
    const editingCell = editedCell();
    const editor = editingCell.querySelector('input');
    expect(editor).not.toBeNull();
    expect(document.activeElement).toBe(editor);

    await clickSearch();
    expect(editedCell().querySelector('input')).toBeNull();
    await user.keyboard('description');
    await settle();

    expect({
      query: search.value,
      focused: document.activeElement === search,
      name: row.get(SEARCH_TITLE),
      description: row.get(SEARCH_DESCRIPTION),
      saves: save.mock.calls.length,
    }).toEqual({
      query: 'description',
      focused: true,
      name: 'needle',
      description: SAVED_DESCRIPTION,
      saves: 0,
    });
    expect(editedCell().querySelector('input')).toBeNull();
    await waitFor(() => expect(editedCell()).not.toBe(editingCell));

    const filteredCell = editedCell();
    await user.click(screen.getByTitle('Clear'));
    await waitFor(() => expect(search.value).toBe(''));
    await waitFor(() => expect(editedCell()).not.toBe(filteredCell));
    expect(document.activeElement).toBe(search);
    expect(row.get(SEARCH_TITLE)).toBe('needle');
    expect(row.get(SEARCH_DESCRIPTION)).toBe(SAVED_DESCRIPTION);
    expect(save).not.toHaveBeenCalled();

    await user.keyboard('no-match');
    await waitFor(() =>
      expect(screen.getByRole('status').textContent).toBe(
        'No rows show “no-match”.',
      ),
    );
    expect(screen.getByRole('grid').getAttribute('aria-rowcount')).toBe('0');
    expect(document.querySelector('[aria-rowindex="2"]')).toBeNull();

    await user.click(screen.getByTitle('Clear'));
    await waitFor(() => expect(search.value).toBe(''));
    await waitFor(() => expect(screen.queryByRole('status')).toBeNull());
    await waitFor(() =>
      expect(searchDescriptionCell().textContent).toContain(SAVED_DESCRIPTION),
    );
    expect(document.activeElement).toBe(search);
    expect(row.get(SEARCH_TITLE)).toBe('needle');
    expect(row.get(SEARCH_DESCRIPTION)).toBe(SAVED_DESCRIPTION);
    expect(save).not.toHaveBeenCalled();

    await user.click(editedCell());
    const repeatedEditingCell = editedCell();
    const repeatedEditor = repeatedEditingCell.querySelector('input');
    expect(repeatedEditor).not.toBeNull();
    expect(document.activeElement).toBe(repeatedEditor);

    await clickSearch();
    expect(editedCell().querySelector('input')).toBeNull();
    await user.keyboard('description');
    await settle();

    expect({
      query: search.value,
      focused: document.activeElement === search,
      name: row.get(SEARCH_TITLE),
      description: row.get(SEARCH_DESCRIPTION),
      saves: save.mock.calls.length,
    }).toEqual({
      query: 'description',
      focused: true,
      name: 'needle',
      description: SAVED_DESCRIPTION,
      saves: 0,
    });
    expect(editedCell().querySelector('input')).toBeNull();
    await waitFor(() => expect(editedCell()).not.toBe(repeatedEditingCell));

    const repeatedFilteredCell = editedCell();
    await user.click(screen.getByTitle('Clear'));
    await waitFor(() => expect(search.value).toBe(''));
    await waitFor(() => expect(editedCell()).not.toBe(repeatedFilteredCell));
    expect(document.activeElement).toBe(search);
    expect(row.get(SEARCH_TITLE)).toBe('needle');
    expect(row.get(SEARCH_DESCRIPTION)).toBe(SAVED_DESCRIPTION);
    expect(save).not.toHaveBeenCalled();
  });

  it('keeps the full query and Description intact through a real filter row remount', async () => {
    const user = userEvent.setup();
    const { row, save } = await renderSearchableTable();
    const search = screen.getByRole('textbox', {
      name: 'Find rows containing text',
    }) as HTMLInputElement;

    await user.click(searchDescriptionCell());
    await user.keyboard('{Enter}');
    expect(searchDescriptionCell().querySelector('input')).not.toBeNull();

    const selectedCell = searchDescriptionCell();
    await user.click(search);
    expect(document.activeElement).toBe(search);
    expect(searchDescriptionCell().querySelector('input')).toBeNull();

    await user.keyboard('n');
    await waitFor(() => expect(search.value).toBe('n'));
    await waitFor(() => expect(searchDescriptionCell()).not.toBe(selectedCell));

    await user.keyboard('eedle');
    await settle();

    expect({
      query: search.value,
      focused: document.activeElement === search,
      description: row.get(SEARCH_DESCRIPTION),
      saves: save.mock.calls.length,
    }).toEqual({
      query: 'needle',
      focused: true,
      description: SAVED_DESCRIPTION,
      saves: 0,
    });

    const filteredCell = searchDescriptionCell();
    await user.click(screen.getByTitle('Clear'));
    await waitFor(() => expect(search.value).toBe(''));
    await waitFor(() => expect(searchDescriptionCell()).not.toBe(filteredCell));
    expect(document.activeElement).toBe(search);

    await user.keyboard('needle');
    await settle();

    expect({
      query: search.value,
      focused: document.activeElement === search,
      description: row.get(SEARCH_DESCRIPTION),
      saves: save.mock.calls.length,
    }).toEqual({
      query: 'needle',
      focused: true,
      description: SAVED_DESCRIPTION,
      saves: 0,
    });
  });
});

describe('typing on a selected date cell (#1822)', () => {
  it('opens the editor with the character and keeps the stored date', async () => {
    const row = await renderDateCell();

    await typeOnSelectedCell('2');

    const input = screen.getByRole('textbox') as HTMLInputElement;
    expect(input.value).toBe('2');
    expect(row.get(DAY)).toBe(STORED);
  });

  it('keeps the stored date when the edit is abandoned with Escape', async () => {
    const row = await renderDateCell();

    await typeOnSelectedCell('2');
    await act(async () => {
      fireEvent.keyDown(screen.getByRole('textbox'), { key: 'Escape' });
    });
    await settle();

    expect(screen.queryByRole('textbox')).toBeNull();
    expect(row.get(DAY)).toBe(STORED);
    expect(editedCell().textContent).toMatch(/25.*2026|2026.*25/);
  });

  it('stores the date once the typed text is committed', async () => {
    const row = await renderDateCell();

    await typeOnSelectedCell('2');
    const input = screen.getByRole('textbox');
    fireEvent.change(input, { target: { value: '2026-10-2' } });
    await act(async () => {
      fireEvent.keyDown(input, { key: 'Enter' });
    });

    await waitFor(() => expect(row.get(DAY)).toBe('2026-10-02'));
  });
});

const COUNT = 'https://example.com/properties/count';
const RATIO = 'https://example.com/properties/ratio';
const AT = 'https://example.com/properties/at';

const countProperty: Property = {
  subject: COUNT,
  datatype: Datatype.INTEGER,
  shortname: 'count',
  description: '',
};

const ratioProperty: Property = {
  subject: RATIO,
  datatype: Datatype.FLOAT,
  shortname: 'ratio',
  description: '',
};

const atProperty: Property = {
  subject: AT,
  datatype: Datatype.TIMESTAMP,
  shortname: 'at',
  description: '',
};

// Local time, so the editor's `datetime-local` text is predictable.
const STORED_AT = new Date(2026, 8, 25, 14, 30).getTime();

async function pressOnEditor(key: string, target: Element) {
  await act(async () => {
    fireEvent.keyDown(target, { key });
  });
  await settle();
}

function timestampInput(): HTMLInputElement {
  const input = editedCell().querySelector('input[type="datetime-local"]');

  if (!input) {
    throw new Error('No timestamp editor open');
  }

  return input as HTMLInputElement;
}

describe('typing on a selected timestamp cell (#1825)', () => {
  it('opens the editor on the stored time and stores no digit', async () => {
    const row = await renderCell(atProperty, STORED_AT);

    await typeOnSelectedCell('5');

    expect(row.get(AT)).toBe(STORED_AT);
    expect(timestampInput().value).toBe('2026-09-25T14:30');
  });

  it('keeps the stored time when the edit is abandoned with Escape', async () => {
    const row = await renderCell(atProperty, STORED_AT);

    await typeOnSelectedCell('5');
    await pressOnEditor('Escape', timestampInput());

    expect(editedCell().querySelector('input')).toBeNull();
    expect(row.get(AT)).toBe(STORED_AT);
  });
});

describe('typing on a selected number cell (#1825)', () => {
  it('ignores a letter on an integer cell and opens on the stored value', async () => {
    const row = await renderCell(countProperty, 42);

    await typeOnSelectedCell('a');

    expect(row.get(COUNT)).toBe(42);
    expect((screen.getByRole('textbox') as HTMLInputElement).value).toBe('42');

    await pressOnEditor('Escape', screen.getByRole('textbox'));
    expect(row.get(COUNT)).toBe(42);
  });

  it('seeds an integer cell with a digit, and Enter stores it', async () => {
    const row = await renderCell(countProperty, 42);

    await typeOnSelectedCell('7');

    const input = screen.getByRole('textbox') as HTMLInputElement;
    expect(input.value).toBe('7');
    expect(row.get(COUNT)).toBe(42);

    await pressOnEditor('Enter', input);
    await waitFor(() => expect(row.get(COUNT)).toBe(7));
  });

  it('keeps the stored integer when a seeded edit is abandoned with Escape', async () => {
    const row = await renderCell(countProperty, 42);

    await typeOnSelectedCell('7');
    await pressOnEditor('Escape', screen.getByRole('textbox'));

    expect(screen.queryByRole('textbox')).toBeNull();
    expect(row.get(COUNT)).toBe(42);
  });

  it('stores nothing for a partial number, only the finished one', async () => {
    const row = await renderCell(ratioProperty, 1.5);

    await typeOnSelectedCell('-');

    const input = screen.getByRole('textbox') as HTMLInputElement;
    expect(input.value).toBe('-');
    expect(row.get(RATIO)).toBe(1.5);

    fireEvent.change(input, { target: { value: '-2.' } });
    await settle();
    expect(row.get(RATIO)).toBe(1.5);

    fireEvent.change(input, { target: { value: '-2.25' } });
    await pressOnEditor('Tab', input);
    await waitFor(() => expect(row.get(RATIO)).toBe(-2.25));
  });

  it('ignores a letter on a float cell', async () => {
    const row = await renderCell(ratioProperty, 1.5);

    await typeOnSelectedCell('x');

    expect(row.get(RATIO)).toBe(1.5);
    expect((screen.getByRole('textbox') as HTMLInputElement).value).toBe('1.5');
  });
});
