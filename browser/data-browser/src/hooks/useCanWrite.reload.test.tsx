// @vitest-environment jsdom
// @wc-ignore-file
// After a reload or navigation the page mounts while the resource, its drive
// or the agent are still arriving. `useCanWrite` must re-check when they do,
// not keep the answer it computed from the empty first render.
import { describe, expect, it } from 'vitest';
import React from 'react';
import { act, renderHook, waitFor } from '@testing-library/react';
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

const SUBJECT = 'did:ad:drive-under-test';
const DRIVE = 'https://atomicdata.dev/properties/drive';

async function newAgent(): Promise<Agent> {
  const keys = await Agent.generateKeyPair();

  return new Agent(
    new JSCryptoProvider(keys.privateKey),
    `did:ad:agent:${keys.publicKey}`,
  );
}

async function setup({ signedIn = true } = {}) {
  const agent = await newAgent();
  const store = new Store({
    serverUrl: 'https://atomic.example',
    connect: false,
  });
  // Nothing outside the store answers; what a test adds is all there is.
  store.injectFetch(async () => new Response('Not found', { status: 404 }));

  if (signedIn) store.setAgent(agent);

  const wrapper = ({ children }: { children: React.ReactNode }) => (
    <StoreContext value={store}>{children}</StoreContext>
  );

  return { agent, store, wrapper };
}

function loadingResource(store: Store): Resource {
  const resource = new Resource(SUBJECT);
  resource.setStore(store);
  resource.loading = true;

  return resource;
}

describe('useCanWrite after a reload', () => {
  it('becomes true when the loaded resource (a new object) grants the agent write', async () => {
    const { agent, store, wrapper } = await setup();
    const { result, rerender } = renderHook(
      ({ resource }) => useCanWrite(resource),
      { initialProps: { resource: loadingResource(store) }, wrapper },
    );
    await waitFor(() => expect(result.current).toBe(false));

    const loaded = new Resource(SUBJECT);
    loaded.setStore(store);
    loaded.applyHydratedValues([[core.properties.write, [agent.subject!]]]);
    loaded.loading = false;
    await expect(loaded.canWrite(agent.subject)).resolves.toEqual([
      true,
      undefined,
    ]);
    rerender({ resource: loaded });

    await waitFor(() => expect(result.current).toBe(true));
  });

  it('becomes true when the same resource object is filled in place', async () => {
    const { agent, store, wrapper } = await setup();
    const resource = loadingResource(store);
    const { result, rerender } = renderHook(
      ({ resource: r }) => useCanWrite(r),
      { initialProps: { resource }, wrapper },
    );
    await waitFor(() => expect(result.current).toBe(false));

    resource.applyHydratedValues([[core.properties.write, [agent.subject!]]]);
    resource.loading = false;
    rerender({ resource });

    await waitFor(() => expect(result.current).toBe(true));
  });

  it('becomes true when the drive it lives in receives the grant later', async () => {
    const { agent, store, wrapper } = await setup();
    const drive = new Resource('atomic:resource:owner-drive');
    await drive.set(core.properties.isA, [server.classes.drive], false);
    store.addResource(drive);
    const article = new Resource('atomic:resource:article-in-drive');
    await article.set(DRIVE, drive.subject, false);
    store.addResource(article);
    const { result } = renderHook(() => useCanWrite(article), { wrapper });
    await waitFor(() => expect(result.current).toBe(false));

    // The drive arrives again from the server, now with the owner's grant;
    // the component showing the article does not re-render by itself. An
    // incoming update is merged without the last-commit shortcut, as
    // `Store.applyIncoming` does.
    const granted = new Resource(drive.subject);
    await granted.set(core.properties.isA, [server.classes.drive], false);
    await granted.set(core.properties.write, [agent.subject!], false);
    await act(async () => {
      store.addResource(granted, { skipCommitCompare: true });
    });

    await waitFor(() => expect(result.current).toBe(true));
  });

  it('becomes true when the agent signs in after the page mounted', async () => {
    const { agent, store, wrapper } = await setup({ signedIn: false });
    const drive = new Resource('atomic:resource:signed-in-later');
    await drive.set(core.properties.isA, [server.classes.drive], false);
    await drive.set(core.properties.write, [agent.subject!], false);
    store.addResource(drive);
    const { result } = renderHook(() => useCanWrite(drive), { wrapper });
    expect(result.current).toBe(false);

    act(() => store.setAgent(agent));

    await waitFor(() => expect(result.current).toBe(true));
  });
});
