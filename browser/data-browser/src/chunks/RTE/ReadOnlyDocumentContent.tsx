import { useEffect, useMemo } from 'react';
import { EditorContent, useEditor } from '@tiptap/react';
import { MarkdownManager } from '@tiptap/markdown';
import type { JSONContent } from '@tiptap/core';
import type { Resource, Store } from '@tomic/react';
import { styled } from 'styled-components';
import Markdown from '@components/datatypes/Markdown';
import { getCollaborativeEditorSchema } from './getCollaborativeEditorSchema';
import {
  hasDocumentContent,
  readDocumentV2TiptapJson,
} from './readDocumentV2TiptapJson';
import { loroDocToMarkdownString } from '@chunks/ResourceDiff/loroDocToMarkdownForDiff';
interface ReadOnlyDocumentContentProps {
  resource: Resource;
  store: Store;
}

const EMPTY_DOCUMENT: JSONContent = { type: 'doc', content: [] };

export function ReadOnlyDocumentContent({
  resource,
  store,
}: ReadOnlyDocumentContentProps) {
  const { schema, extensions } = useMemo(
    () => getCollaborativeEditorSchema(store),
    [store],
  );
  const read = useMemo(
    () => readDocumentV2TiptapJson(resource, store, schema),
    [resource, schema, store],
  );
  const content = read.ok ? read.docJson : EMPTY_DOCUMENT;
  const editor = useEditor({
    extensions,
    content,
    editable: false,
    immediatelyRender: false,
  });

  useEffect(() => {
    if (!editor || editor.isDestroyed || !read.ok) return;

    editor.commands.setContent(read.docJson, { emitUpdate: false });
  }, [editor, read]);

  const fallbackMarkdown = useMemo(() => {
    const doc = resource.getLoroDoc();

    if (!doc || read.ok || hasDocumentContent(resource)) return '';

    return loroDocToMarkdownString(
      doc,
      schema,
      new MarkdownManager({ extensions }),
    );
  }, [extensions, read, resource, schema]);

  if (!resource.hasLoroBody()) return null;

  if (read.ok) {
    return editor ? (
      <ReadOnlyWrap>
        <EditorContent editor={editor} />
      </ReadOnlyWrap>
    ) : null;
  }

  if (hasDocumentContent(resource)) {
    return <div role='alert'>{read.error}</div>;
  }

  return fallbackMarkdown ? <Markdown text={fallbackMarkdown} /> : null;
}

const ReadOnlyWrap = styled.div`
  .tiptap {
    max-width: min(100%, 75ch);
    outline: none;
  }

  .tiptap > :first-child {
    margin-top: 0;
  }

  .tiptap .tiptap-image {
    max-width: 100%;
    height: auto;
  }
`;
