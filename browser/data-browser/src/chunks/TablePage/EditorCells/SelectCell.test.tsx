// @vitest-environment jsdom
// @wc-ignore-file
import React from 'react';
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest';
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from '@testing-library/react';
import { ThemeProvider } from 'styled-components';
import {
  Resource,
  Store,
  StoreContext,
  core,
  type JSONValue,
} from '@tomic/react';
import { buildTheme } from '../../../styling';
import { SelectCell } from './SelectCell';

const PROPERTY = 'atomic:resource:select-property';
// Opaque like real tag DIDs: an unloaded tag's subject must not contain the
// name being searched for.
const DREAMY = 'atomic:resource:3kq9xv';
const CALM = 'atomic:resource:7hz2mw';

afterEach(cleanup);

// jsdom has no Popover API; the tag list renders in a popover.
const platformPopover = {
  show: HTMLElement.prototype.showPopover,
  hide: HTMLElement.prototype.hidePopover,
};

beforeAll(() => {
  HTMLElement.prototype.showPopover = () => {};
  HTMLElement.prototype.hidePopover = () => {};
});

afterAll(() => {
  HTMLElement.prototype.showPopover = platformPopover.show;
  HTMLElement.prototype.hidePopover = platformPopover.hide;
});

/** A Store holding the select Property; its tags are fetched, and their
 * fetches never answer, so they stay loading until a test adds them. */
async function storeWithProperty(): Promise<Store> {
  const store = new Store({
    serverUrl: 'https://atomic.example',
    connect: false,
  });
  store.injectFetch(() => new Promise<Response>(() => {}));
  const property = new Resource(PROPERTY);
  await property.set(core.properties.allowsOnly, [DREAMY, CALM], false);
  store.addResource(property);

  return store;
}

async function loadTag(store: Store, subject: string, name: string) {
  const tag = new Resource(subject);
  await tag.set(core.properties.name, name, false);
  await act(async () => {
    store.addResource(tag);
  });
}

function renderEditor(store: Store, value: JSONValue) {
  const onChange = vi.fn();
  render(
    <StoreContext value={store}>
      <ThemeProvider theme={buildTheme(false, '#1b50d8')}>
        <SelectCell.Edit
          value={value}
          onChange={onChange}
          property={PROPERTY}
          resource={new Resource('atomic:resource:row')}
        />
      </ThemeProvider>
    </StoreContext>,
  );
  const input = screen.getByPlaceholderText('filter tags');

  return { onChange, input };
}

describe('SelectCell editor', () => {
  it('keeps the existing tags when Enter is pressed and no option matches', async () => {
    const store = await storeWithProperty();
    await loadTag(store, CALM, 'calm');
    const { onChange, input } = renderEditor(store, [CALM]);

    fireEvent.change(input, { target: { value: 'nothing like this' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    expect(onChange).not.toHaveBeenCalled();
  });

  it('offers a tag by its name once the tag resource has loaded', async () => {
    const store = await storeWithProperty();
    const { onChange, input } = renderEditor(store, []);

    fireEvent.change(input, { target: { value: 'dreamy' } });
    // Not loaded yet: its name is unknown, so nothing matches.
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(onChange).not.toHaveBeenCalled();

    await loadTag(store, DREAMY, 'dreamy');
    fireEvent.keyDown(input, { key: 'Enter' });

    expect(onChange).toHaveBeenCalledWith([DREAMY]);
  });
});
