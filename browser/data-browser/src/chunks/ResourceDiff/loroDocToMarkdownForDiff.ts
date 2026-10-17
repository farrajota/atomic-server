import { MarkdownManager } from '@tiptap/markdown';
import type { Schema } from '@tiptap/pm/model';
import type { LoroDoc } from 'loro-crdt';

function extractTextFromNode(node: unknown): string {
  if (!node || typeof node !== 'object') return '';

  const n = node as { text?: unknown; content?: unknown };

  if (typeof n.text === 'string') return n.text;

  if (Array.isArray(n.content)) {
    return n.content.map(extractTextFromNode).join(' ');
  }

  return '';
}

/** Serializes legacy non-DocumentV2 containers for the existing read-only fallback.
 * Falls back to plain text if Markdown serialization fails.
 */
export function loroDocToMarkdownString(
  loroDoc: LoroDoc,
  schema: Schema,
  mdManager: MarkdownManager,
): string {
  try {
    const json = loroDoc.toJSON()?.doc;
    if (!json) return '';

    return mdManager.serialize(json);
  } catch {
    const docRoot = loroDoc.toJSON()?.doc;

    return docRoot ? extractTextFromNode(docRoot) : '';
  }
}
