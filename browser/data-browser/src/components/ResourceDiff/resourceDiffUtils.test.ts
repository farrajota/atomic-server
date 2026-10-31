import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { beforeAll, describe, expect, it } from 'vitest';
import { dataBrowser, LoroLoader, Resource } from '@tomic/react';
import initWebLoro from 'loro-crdt/web';
import type {
  LoroList as LoroListType,
  LoroMap as LoroMapType,
  LoroText as LoroTextType,
} from 'loro-crdt';
import { useResourceDiff } from './resourceDiffUtils';

beforeAll(async () => {
  const wasm = await readFile(
    createRequire(import.meta.url).resolve('loro-crdt/web/loro_wasm_bg.wasm'),
  );
  await initWebLoro({ module_or_path: wasm });
  await LoroLoader.initializeLoro();
});

describe('useResourceDiff', () => {
  it('reports native body edits and formatting changes without commit bookkeeping', () => {
    const subject = 'https://example.com/history-diff-document';
    const lastCommit = 'https://atomicdata.dev/properties/lastCommit';
    const oldResource = new Resource(subject);
    const newResource = new Resource(subject);

    oldResource.applyHydratedValues([[lastCommit, 'commit-before']]);
    newResource.applyHydratedValues([[lastCommit, 'commit-after']]);
    oldResource
      .getLoroDoc()!
      .getMap('doc')
      .set('content', [
        {
          type: 'paragraph',
          content: [{ type: 'text', text: 'Same body' }],
        },
      ]);
    newResource
      .getLoroDoc()!
      .getMap('doc')
      .set('content', [
        {
          type: 'paragraph',
          content: [
            { type: 'text', text: 'Same ' },
            { type: 'text', text: 'body', marks: [{ type: 'bold' }] },
          ],
        },
      ]);

    const diff = useResourceDiff(oldResource, newResource);

    expect(diff.bodyChanged).toBe(true);
    expect(diff.changedProps).toEqual([]);
    expect(diff.changedProps).not.toContain(lastCommit);
  });
  it('detects native LoroText mark-only differences when body JSON is equal', () => {
    const subject = 'https://example.com/native-history-formatting-diff';

    const buildBody = (resource: Resource, color?: string) => {
      const { LoroList, LoroMap, LoroText } = LoroLoader.Loro;
      const doc = resource.getLoroDoc()!;
      doc.configTextStyle({ textStyle: { expand: 'after' } });
      const root = doc.getMap('doc');
      root.set('nodeName', 'doc');
      root.setContainer('children', new LoroList());

      const blocks = root.get('children') as LoroListType;
      blocks.insertContainer(0, new LoroMap());
      const paragraph = blocks.get(0) as LoroMapType;
      paragraph.set('nodeName', 'paragraph');
      paragraph.set('textAlign', 'center');
      paragraph.setContainer('children', new LoroList());

      const paragraphChildren = paragraph.get('children') as LoroListType;
      paragraphChildren.insertContainer(0, new LoroText());
      const text = paragraphChildren.get(0) as LoroTextType;
      text.insert(0, 'same native text');

      if (color) {
        text.applyDelta([
          {
            retain: text.length,
            attributes: { textStyle: { color } },
          },
        ]);
      }

      return { doc, text };
    };

    const oldResource = new Resource(subject);
    const newResource = new Resource(subject);
    const oldBody = buildBody(oldResource);
    const newBody = buildBody(newResource, '#dc2626');

    expect(newBody.doc.toJSON()?.doc).toEqual(oldBody.doc.toJSON()?.doc);
    expect(newBody.text.toDelta()).not.toEqual(oldBody.text.toDelta());

    const diff = useResourceDiff(oldResource, newResource);

    expect(diff.bodyChanged).toBe(true);
    expect(diff.changedProps).toEqual([]);
  });

  it('continues to diff a legacy documentContent property as a property', () => {
    const subject = 'https://example.com/legacy-document';
    const bodyProperty = dataBrowser.properties.documentContent;
    const oldResource = new Resource(subject);
    const newResource = new Resource(subject);

    oldResource.applyHydratedValues([[bodyProperty, 'Old property body']]);
    newResource.applyHydratedValues([[bodyProperty, 'New property body']]);

    const diff = useResourceDiff(oldResource, newResource);

    expect(diff.changedProps).toEqual([bodyProperty]);
    expect(diff.bodyChanged).toBe(false);
  });
});
