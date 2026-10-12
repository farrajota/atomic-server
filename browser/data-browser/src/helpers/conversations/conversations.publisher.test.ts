import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import * as loroWeb from 'loro-crdt/web';
import { beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  Agent,
  conversations,
  core,
  Datatype,
  JSCryptoProvider,
  LoroLoader,
  RequestCancelledError,
  Resource,
  Store,
} from '@tomic/react';
import type { Commit } from '@tomic/react';
import { encryptionKeyFor } from './conversationCrypto';
import type * as ConversationCrypto from './conversationCrypto';
import { ensureEncryptionKey } from './conversations';

vi.mock('./conversationCrypto', async importOriginal => {
  const actual = await importOriginal<typeof ConversationCrypto>();

  return {
    ...actual,
    encryptionKeyFor: vi.fn(async () => 'my-key'),
  };
});

beforeAll(async () => {
  // App Vite resolves Loro to its web build; initialize that real module with
  // disk bytes so Node does not try to fetch a file URL.
  const wasm = await readFile(
    createRequire(import.meta.url).resolve('loro-crdt/web/loro_wasm_bg.wasm'),
  );
  const web = loroWeb as unknown as {
    default: (opts: { module_or_path: Uint8Array }) => Promise<unknown>;
  };
  await web.default({ module_or_path: wasm });
  await LoroLoader.initializeLoro();
});

const OWN_KEY = 'a'.repeat(43);
const KEY_PROPERTY = conversations.properties.encryptionKey;

interface SetupOptions {
  cachedKey?: string;
  authoritativeKey?: string;
  authoritativeName?: string;
  responseStatus?: number;
  cachedError?: Error;
}

async function setup(options: SetupOptions = {}) {
  const keys = await Agent.generateKeyPair();
  const agent = new Agent(
    new JSCryptoProvider(keys.privateKey),
    `atomic:agent:${keys.publicKey}`,
  );
  const subject = agent.subject!;
  const store = new Store({ serverUrl: 'https://example.com', connect: false });
  store.setServerConnected(true);

  const agentClass = new Resource(core.classes.agent);
  agentClass.applyHydratedValues([
    [core.properties.isA, [core.classes.class]],
    [core.properties.shortname, 'agent'],
  ]);
  agentClass.loading = false;
  store.addResource(agentClass);

  for (const [propertySubject, shortname, datatype] of [
    [KEY_PROPERTY, 'encryptionKey', Datatype.STRING],
    [core.properties.isA, 'isA', Datatype.RESOURCEARRAY],
    [core.properties.name, 'name', Datatype.STRING],
  ] as const) {
    const property = new Resource(propertySubject);
    property.applyHydratedValues([
      [core.properties.isA, [core.classes.property]],
      [core.properties.shortname, shortname],
      [core.properties.datatype, datatype],
    ]);
    property.loading = false;
    store.addResource(property);
  }

  const requestedSubjects: string[] = [];
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = new URL(String(input));
    const requestedSubject = url.searchParams.get('subject') ?? url.href;
    requestedSubjects.push(requestedSubject);

    if (requestedSubject !== subject) {
      return new Response(`Unexpected fixture request: ${requestedSubject}`, {
        status: 404,
      });
    }

    if (
      options.responseStatus !== undefined &&
      options.responseStatus !== 200
    ) {
      return new Response('Agent resource unavailable', {
        status: options.responseStatus,
      });
    }

    const responseResource: Record<string, unknown> = {
      '@id': subject,
      [core.properties.isA]: [core.classes.agent],
    };

    if (options.authoritativeKey !== undefined) {
      responseResource[KEY_PROPERTY] = options.authoritativeKey;
    }

    if (options.authoritativeName !== undefined) {
      responseResource[core.properties.name] = options.authoritativeName;
    }

    return new Response(JSON.stringify(responseResource), {
      status: 200,
      headers: { 'content-type': 'application/ad+json' },
    });
  });
  store.injectFetch(fetchMock);
  agent.client.setFetch(fetchMock);
  store.setAgent(agent);

  const resource = new Resource(subject);
  resource.setStore(store);
  resource.loading = false;

  resource.applyHydratedValues([[core.properties.isA, [core.classes.agent]]]);

  if (options.cachedKey !== undefined) {
    resource.applyHydratedValues([[KEY_PROPERTY, options.cachedKey]]);
  }

  if (options.cachedError) resource.setError(options.cachedError);
  store.resources.set(subject, resource);

  const posted: Commit[] = [];
  const postCommitSpy = vi.fn(async (commit: Commit) => {
    const created = {
      ...commit,
      id: `https://example.com/commits/${commit.signature}`,
    } as Commit;
    posted.push(created);

    return created;
  });
  // Keep Resource.save real and intercept only its commit HTTP boundary.
  const sdkClient = store as unknown as {
    client: { postCommit: typeof postCommitSpy };
  };
  sdkClient.client.postCommit = postCommitSpy;

  return { store, agent, resource, fetchMock, requestedSubjects, posted };
}

function expectPublishedKey(posted: Commit[], subject: string) {
  expect(posted.map(commit => commit.subject)).toEqual([subject]);
  expect(posted[0].loroUpdate).toBeInstanceOf(Uint8Array);

  // Materialize exactly the bytes posted to the host, not the sender's cache.
  const received = new Resource(posted[0].subject);
  received.importLoroUpdate(posted[0].loroUpdate!, true);
  expect(received.get(KEY_PROPERTY)).toBe(OWN_KEY);
  expect(received.get(core.properties.isA)).toEqual([core.classes.agent]);

  return received;
}

describe('ensureEncryptionKey with SDK resources', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(encryptionKeyFor).mockResolvedValue(OWN_KEY);
  });

  it('keeps a matching cached key on the no-fetch path', async () => {
    const { store, agent, resource, fetchMock, posted } = await setup({
      cachedKey: OWN_KEY,
      responseStatus: 404,
    });

    await expect(ensureEncryptionKey(store, agent)).resolves.toBe(OWN_KEY);

    expect(resource.get(KEY_PROPERTY)).toBe(OWN_KEY);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(posted).toEqual([]);
    expect(store.outbox.size).toBe(0);
  });

  it('reads a missing cached key without publishing or changing the cache when the existing Agent matches', async () => {
    const { store, agent, resource, requestedSubjects, posted } = await setup({
      authoritativeKey: OWN_KEY,
    });

    await expect(ensureEncryptionKey(store, agent)).resolves.toBe(OWN_KEY);

    expect(resource.get(KEY_PROPERTY)).toBeUndefined();
    expect(requestedSubjects).toEqual([agent.subject]);
    expect(posted).toEqual([]);
    expect(store.outbox.size).toBe(0);
  });

  it('reads a different cached key without publishing or changing the cache when the existing Agent already matches', async () => {
    const { store, agent, resource, requestedSubjects, posted } = await setup({
      cachedKey: 'old-key',
      authoritativeKey: OWN_KEY,
    });

    await expect(ensureEncryptionKey(store, agent)).resolves.toBe(OWN_KEY);

    expect(resource.get(KEY_PROPERTY)).toBe('old-key');
    expect(requestedSubjects).toEqual([agent.subject]);
    expect(posted).toEqual([]);
    expect(store.outbox.size).toBe(0);
  });

  it('publishes the first key on an existing Agent with no encryption key', async () => {
    const { store, agent, resource, requestedSubjects, posted } = await setup();

    await expect(ensureEncryptionKey(store, agent)).resolves.toBe(OWN_KEY);

    expect(resource.get(KEY_PROPERTY)).toBe(OWN_KEY);
    expect(requestedSubjects).toEqual([agent.subject]);
    expectPublishedKey(posted, agent.subject!);
    expect(store.outbox.size).toBe(0);
  });

  it('updates a changed authoritative key once', async () => {
    const { store, agent, resource, requestedSubjects, posted } = await setup({
      cachedKey: 'old-key',
      authoritativeKey: 'old-key',
    });

    await expect(ensureEncryptionKey(store, agent)).resolves.toBe(OWN_KEY);

    expect(resource.get(KEY_PROPERTY)).toBe(OWN_KEY);
    expect(requestedSubjects).toEqual([agent.subject]);
    expectPublishedKey(posted, agent.subject!);
    expect(store.outbox.size).toBe(0);
  });

  it('preserves an authoritative Resource error instead of publishing to a missing Agent', async () => {
    const { store, agent, resource, requestedSubjects, posted } = await setup({
      responseStatus: 404,
    });
    const fetchResourceHTTP = vi.spyOn(agent.client, 'fetchResourceHTTP');

    let thrown: unknown;

    try {
      await ensureEncryptionKey(store, agent);
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(Error);
    const { resource: authoritativeResource } =
      await fetchResourceHTTP.mock.results[0].value;
    expect(thrown).toBe(authoritativeResource.error);
    expect(resource.error).toBeUndefined();
    expect(requestedSubjects).toEqual([agent.subject]);
    expect(resource.get(KEY_PROPERTY)).toBeUndefined();
    expect(posted).toEqual([]);
    expect(store.outbox.size).toBe(0);
  });

  it('preserves a rejected authoritative fetch without publishing', async () => {
    const { store, agent, resource, posted } = await setup();
    const fetchError = new Error('authoritative fetch rejected');
    // Inject rejection at the SDK HTTP boundary because injected fetch errors
    // resolve as error Resources.
    vi.spyOn(agent.client, 'fetchResourceHTTP').mockImplementation(async () => {
      throw fetchError;
    });

    await expect(ensureEncryptionKey(store, agent)).rejects.toBe(fetchError);

    expect(resource.get(KEY_PROPERTY)).toBeUndefined();
    expect(posted).toEqual([]);
    expect(store.outbox.size).toBe(0);
  });

  it('preserves a pending profile edit when the authoritative encryption key already matches', async () => {
    const { store, agent, resource, requestedSubjects, posted } = await setup({
      cachedKey: 'old-key',
      authoritativeKey: OWN_KEY,
      authoritativeName: 'Server profile',
    });
    await resource.set(core.properties.name, 'Pending profile', false);
    expect(resource.hasUnsavedChanges()).toBe(true);

    await expect(ensureEncryptionKey(store, agent)).resolves.toBe(OWN_KEY);

    expect(resource.get(core.properties.name)).toBe('Pending profile');
    expect(resource.get(KEY_PROPERTY)).toBe('old-key');
    expect(resource.hasUnsavedChanges()).toBe(true);
    expect(requestedSubjects).toEqual([agent.subject]);
    expect(posted).toEqual([]);
    expect(store.outbox.size).toBe(0);
  });

  it('retains a pending profile edit in the real publication payload when the authoritative key differs', async () => {
    const { store, agent, resource, requestedSubjects, posted } = await setup({
      cachedKey: 'old-key',
      authoritativeKey: 'old-key',
      authoritativeName: 'Server profile',
    });
    await resource.set(core.properties.name, 'Pending profile', false);

    await expect(ensureEncryptionKey(store, agent)).resolves.toBe(OWN_KEY);

    expect(resource.get(core.properties.name)).toBe('Pending profile');
    expect(resource.get(KEY_PROPERTY)).toBe(OWN_KEY);
    expect(requestedSubjects).toEqual([agent.subject]);
    const received = expectPublishedKey(posted, agent.subject!);
    expect(received.get(core.properties.name)).toBe('Pending profile');
    expect(store.outbox.size).toBe(0);
  });

  it('rejects a cancelled authoritative read without publishing even if its key matches', async () => {
    const { store, agent, resource, posted } = await setup({
      cachedKey: 'old-key',
    });
    const authoritativeResource = new Resource(agent.subject!);
    authoritativeResource.applyHydratedValues([
      [core.properties.isA, [core.classes.agent]],
      [KEY_PROPERTY, OWN_KEY],
    ]);
    // Cancellation is an HTTP-boundary result, not a publishable response.
    vi.spyOn(agent.client, 'fetchResourceHTTP').mockResolvedValue({
      resource: authoritativeResource,
      createdResources: [authoritativeResource],
      cancelled: true,
    });

    await expect(ensureEncryptionKey(store, agent)).rejects.toBeInstanceOf(
      RequestCancelledError,
    );

    expect(resource.get(KEY_PROPERTY)).toBe('old-key');
    expect(posted).toEqual([]);
    expect(store.outbox.size).toBe(0);
  });

  it('preserves a cached own-Agent Resource error without fetching or publishing', async () => {
    const resourceError = new Error('own-Agent Resource failed');
    const { store, agent, fetchMock, posted } = await setup({
      cachedError: resourceError,
    });

    await expect(ensureEncryptionKey(store, agent)).rejects.toBe(resourceError);

    expect(fetchMock).not.toHaveBeenCalled();
    expect(posted).toEqual([]);
    expect(store.outbox.size).toBe(0);
  });
});
