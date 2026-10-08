// @vitest-environment jsdom
// @wc-ignore-file
import React from 'react';
import { describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import {
  Agent,
  JSCryptoProvider,
  Resource,
  Store,
  StoreContext,
  core,
  server,
  useCanWrite,
} from '@tomic/react';

const DRIVE = 'https://atomicdata.dev/properties/drive';

async function newAgent(): Promise<Agent> {
  const keys = await Agent.generateKeyPair();

  return new Agent(
    new JSCryptoProvider(keys.privateKey),
    `did:ad:agent:${keys.publicKey}`,
  );
}

/** A DID drive the owner may write and the reader may only read, with an
 * Article in it, as the browser-invite e2e sets them up. */
async function sharedArticle(owner: Agent, reader: Agent) {
  const store = new Store({
    serverUrl: 'https://atomic.example',
    connect: false,
  });
  store.injectFetch(async () => new Response('Not found', { status: 404 }));
  const drive = new Resource('atomic:resource:invite-drive');
  await drive.set(core.properties.isA, [server.classes.drive], false);
  await drive.set(core.properties.write, [owner.subject!], false);
  await drive.set(
    core.properties.read,
    [owner.subject!, reader.subject!],
    false,
  );
  store.addResource(drive);
  const article = new Resource('atomic:resource:invite-article');
  await article.set(core.properties.parent, drive.subject, false);
  await article.set(DRIVE, drive.subject, false);
  store.addResource(article);

  return { store, article };
}

/** `useCanWrite` for `agent`, once its rights check has settled. */
async function settledCanWrite(store: Store, resource: Resource, agent: Agent) {
  store.setAgent(agent);
  const check = vi.spyOn(resource, 'canWrite');
  const { result } = renderHook(() => useCanWrite(resource), {
    wrapper: ({ children }: { children: React.ReactNode }) => (
      <StoreContext value={store}>{children}</StoreContext>
    ),
  });

  await act(async () => {
    await check.mock.results[0]?.value;
  });

  expect(check).toHaveBeenCalledWith(agent.subject);

  return result.current;
}

describe('useCanWrite', () => {
  it('denies a read-only reader of a DID drive, so no edit controls show', async () => {
    const owner = await newAgent();
    const reader = await newAgent();
    const { store, article } = await sharedArticle(owner, reader);

    expect(await settledCanWrite(store, article, reader)).toBe(false);
  });

  it('allows the agent the drive grants write', async () => {
    const owner = await newAgent();
    const reader = await newAgent();
    const { store, article } = await sharedArticle(owner, reader);

    expect(await settledCanWrite(store, article, owner)).toBe(true);
  });
});
