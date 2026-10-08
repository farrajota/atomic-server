// @vitest-environment jsdom
// @wc-ignore-file
import React from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { Store, StoreContext, useBlobObjectUrl } from '@tomic/react';

const BLOB = `atomic:blob:${'d'.repeat(64)}`;

let created: Blob[];
const platformUrl = {
  create: URL.createObjectURL,
  revoke: URL.revokeObjectURL,
};

/** A Store whose local database holds `bytes` for every blob. */
function storeHolding(bytes: Uint8Array): Store {
  const store = new Store({
    serverUrl: 'https://atomic.example',
    connect: false,
  });
  // A fresh database object per test: object URLs are cached per database.
  vi.spyOn(store, 'getClientDb').mockReturnValue({
    getBlob: async () => bytes,
  } as unknown as NonNullable<ReturnType<Store['getClientDb']>>);

  return store;
}

function renderBlobUrl(store: Store, mimetype: string) {
  return renderHook(() => useBlobObjectUrl(BLOB, undefined, mimetype), {
    wrapper: ({ children }: { children: React.ReactNode }) => (
      <StoreContext value={store}>{children}</StoreContext>
    ),
  });
}

beforeEach(() => {
  created = [];
  // jsdom has no object URLs.
  URL.createObjectURL = vi.fn((blob: Blob) => {
    created.push(blob);

    return `blob:${window.location.origin}/object-${created.length - 1}`;
  });
  URL.revokeObjectURL = vi.fn();
});

afterEach(() => {
  URL.createObjectURL = platformUrl.create;
  URL.revokeObjectURL = platformUrl.revoke;
});

describe('useBlobObjectUrl', () => {
  it('keeps the type of local raster image bytes', async () => {
    const { result } = renderBlobUrl(
      storeHolding(new Uint8Array([1, 2, 3])),
      'image/png',
    );

    await waitFor(() => expect(result.current).toMatch(/^blob:/));
    expect(created[0].type).toBe('image/png');
  });

  it('does not type a local blob URL as a document because the File says so', async () => {
    const { result } = renderBlobUrl(
      storeHolding(new TextEncoder().encode('<script>alert(1)</script>')),
      'text/html',
    );

    await waitFor(() => expect(result.current).toMatch(/^blob:/));
    expect(created[0].type).toBe('application/octet-stream');
  });

  it('shows a local SVG as a data URL instead of a same-origin blob document', async () => {
    const svg = '<svg xmlns="http://www.w3.org/2000/svg"/>';
    const { result } = renderBlobUrl(
      storeHolding(new TextEncoder().encode(svg)),
      'image/svg+xml',
    );

    await waitFor(() =>
      expect(result.current).toBe(`data:image/svg+xml;base64,${btoa(svg)}`),
    );
    expect(created).toHaveLength(0);
  });
});
