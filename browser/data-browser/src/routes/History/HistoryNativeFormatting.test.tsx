// @vitest-environment jsdom
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import type { ReactNode } from 'react';
import initWebLoro from 'loro-crdt/web';
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import type {
  LoroList as LoroListType,
  LoroMap as LoroMapType,
  LoroText as LoroTextType,
} from 'loro-crdt';
import {
  core,
  dataBrowser,
  LoroLoader,
  Resource,
  Store,
  StoreContext,
} from '@tomic/react';
import { ThemeProvider } from 'styled-components';
import { ResourceDiff } from '../../chunks/ResourceDiff/ResourceDiff';
import { useResourceDiff } from '../../components/ResourceDiff/resourceDiffUtils';
import { buildTheme } from '../../styling';
import { VersionContent } from './VersionContent';
import { resourceAtVersion } from './versionHelpers';

// Keep the application shell out of this body-rendering fixture. AppSettings
// imports SideBar, and useCurrentSubject imports ShowRoute; those entries pull
// ResourceCard back into AtomicLink before AtomicLink has initialized. Neither
// application hook is needed by a native paragraph, so fail on unexpected use
// instead of substituting settings, navigation, or any document-rendering logic.
vi.mock('../../helpers/AppSettings', () => ({
  useSettings: () => {
    throw new Error('Native history body fixtures must not read app settings');
  },
}));
vi.mock('../../helpers/useCurrentSubject', () => ({
  useCurrentSubject: () => {
    throw new Error(
      'Native history body fixtures must not read app navigation',
    );
  },
  useSubjectParam: () => {
    throw new Error(
      'Native history body fixtures must not read route parameters',
    );
  },
}));

beforeAll(async () => {
  const wasm = await readFile(
    createRequire(import.meta.url).resolve('loro-crdt/web/loro_wasm_bg.wasm'),
  );
  await initWebLoro({ module_or_path: wasm });
  await LoroLoader.initializeLoro();
});

afterEach(cleanup);

function documentResource(store: Store, subject: string): Resource {
  const resource = new Resource(subject);
  resource.setStore(store);
  resource.applyHydratedValues([
    [core.properties.isA, [dataBrowser.classes.documentV2]],
  ]);

  return resource;
}

function writeNativeBody(
  resource: Resource,
  body: string,
  color: string,
  backgroundColor: string,
) {
  const { LoroList, LoroMap, LoroText } = LoroLoader.Loro;
  const doc = resource.getLoroDoc()!;
  doc.configTextStyle({ textStyle: { expand: 'after' } });
  const root = doc.getMap('doc');
  root.set('nodeName', 'doc');
  root.setContainer('attributes', new LoroMap());
  root.setContainer('children', new LoroList());

  const blocks = root.get('children') as LoroListType;
  blocks.insertContainer(0, new LoroMap());
  const paragraph = blocks.get(0) as LoroMapType;
  paragraph.set('nodeName', 'paragraph');
  const paragraphAttributes = paragraph.setContainer(
    'attributes',
    new LoroMap(),
  );
  paragraphAttributes.set('textAlign', 'center');
  paragraph.setContainer('children', new LoroList());

  const paragraphChildren = paragraph.get('children') as LoroListType;
  paragraphChildren.insertContainer(0, new LoroText());
  const text = paragraphChildren.get(0) as LoroTextType;
  text.insert(0, body);
  text.applyDelta([
    {
      retain: text.length,
      attributes: { textStyle: { color, backgroundColor } },
    },
  ]);
  doc.commit({ message: 'native-formatting' });

  return { doc, text };
}

function renderWithStore(store: Store, children: ReactNode) {
  return render(
    <StoreContext value={store}>
      <ThemeProvider theme={buildTheme(false, '#1b50d8')}>
        {children}
      </ThemeProvider>
    </StoreContext>,
  );
}

describe('native formatting in history', () => {
  it('renders the selected historical body with color, highlight, and alignment', async () => {
    const store = new Store({ serverUrl: 'https://example.com' });
    const source = documentResource(
      store,
      'https://example.com/history-native-formatting',
    );
    const { doc, text } = writeNativeBody(
      source,
      'Selected historical body',
      '#dc2626',
      '#fef08a',
    );
    const selectedVersion = source.getLoroHistory().at(-1)!;
    const selectedResource = resourceAtVersion(source, selectedVersion, store);

    text.delete(0, text.length);
    text.insert(0, 'Current live body');
    doc.commit({ message: 'later-live-edit' });

    renderWithStore(store, <VersionContent resource={selectedResource} />);

    const historicalText = await screen.findByText('Selected historical body');
    expect(screen.queryByText('Current live body')).toBeNull();
    expect(historicalText.closest('p')?.getAttribute('style')).toContain(
      'text-align: center',
    );

    const historicalMark = historicalText.closest('span') as HTMLSpanElement;
    expect(historicalMark.style.color).toBe('rgb(220, 38, 38)');
    expect(historicalMark.style.backgroundColor).toBe('rgb(254, 240, 138)');
  });

  it('shows both native formatting versions in Changes', async () => {
    const store = new Store({ serverUrl: 'https://example.com' });
    const oldResource = documentResource(
      store,
      'https://example.com/history-native-formatting-diff',
    );
    const newResource = documentResource(
      store,
      'https://example.com/history-native-formatting-diff',
    );
    const oldBody = writeNativeBody(
      oldResource,
      'Formatting-only change',
      '#dc2626',
      '#fef08a',
    );
    const newBody = writeNativeBody(
      newResource,
      'Formatting-only change',
      '#2563eb',
      '#bbf7d0',
    );

    expect(newBody.doc.toJSON()?.doc).toEqual(oldBody.doc.toJSON()?.doc);
    expect(newBody.text.toDelta()).not.toEqual(oldBody.text.toDelta());

    const diff = useResourceDiff(oldResource, newResource);
    renderWithStore(store, <ResourceDiff diff={diff} />);

    await waitFor(() =>
      expect(screen.getAllByText('Formatting-only change')).toHaveLength(2),
    );
    const renderedVersions = screen.getAllByText('Formatting-only change');

    const marks = renderedVersions.map(
      element => element.closest('span') as HTMLSpanElement,
    );
    expect(marks.map(mark => mark.style.color)).toEqual(
      expect.arrayContaining(['rgb(220, 38, 38)', 'rgb(37, 99, 235)']),
    );
    expect(marks.map(mark => mark.style.backgroundColor)).toEqual(
      expect.arrayContaining(['rgb(254, 240, 138)', 'rgb(187, 247, 208)']),
    );
  });
});
