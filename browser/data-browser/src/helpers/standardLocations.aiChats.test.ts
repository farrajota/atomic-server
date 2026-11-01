// @wc-ignore-file
import { expect, it, vi } from 'vitest';
import {
  Agent,
  AtomicError,
  ErrorType,
  ai,
  core,
  dataBrowser,
  decodeB64,
  type Store,
} from '@tomic/lib';
import { getOrCreateAiChatsFolder } from './standardLocations';

const secret = Agent.buildSecret(
  btoa('test-only-ai-folder-seed'.padEnd(32, '.')),
  'did:ad:agent:test',
);

interface TestResource {
  subject: string;
  error?: Error;
  get: (prop: string) => unknown;
  set: (prop: string, value: unknown) => Promise<void>;
  save: () => Promise<void>;
  hasClasses: (cls: string) => boolean;
}

function device() {
  const agent = Agent.fromSecret(secret, 'js');
  const resources = new Map<string, TestResource>();

  function resource(subject: string, isFolder = false): TestResource {
    const props = new Map<string, unknown>();

    return {
      subject,
      error: undefined,
      get: (prop: string) => props.get(prop),
      set: vi.fn(async (prop: string, value: unknown) => {
        props.set(prop, value);
      }),
      save: vi.fn(async () => {}),
      hasClasses: (cls: string) =>
        isFolder && cls === dataBrowser.classes.folder,
    };
  }

  resources.set('did:ad:drive', resource('did:ad:drive'));
  const store = {
    getAgent: () => agent,
    isDestroyed: vi.fn(() => false),
    getResource: vi.fn(
      async (subject: string) =>
        resources.get(subject) ?? {
          error: new AtomicError('Missing', ErrorType.NotFound),
        },
    ),
    newResource: vi.fn(async (options: { subject: string }) => {
      const folder = resource(options.subject, true);
      resources.set(folder.subject, folder);

      return folder;
    }),
    notifyResourceManuallyCreated: vi.fn(),
  };

  return { agent, resources, store, typed: store as unknown as Store };
}

it('creates and reuses the signed per-drive folder and persists its pointer', async () => {
  const phone = device();
  const desktop = device();
  const ids = await Promise.all(
    [phone, phone, desktop, desktop].map(d =>
      getOrCreateAiChatsFolder(d.typed, 'did:ad:drive'),
    ),
  );
  expect(new Set(ids).size).toBe(1);

  for (const d of [phone, desktop]) {
    expect(d.store.newResource).toHaveBeenCalledTimes(1);
    const options = d.store.newResource.mock.calls[0][0] as {
      subject: string;
      parent: string;
      genesisCert: { signerPubkey: Uint8Array; parent: string; drive: string };
    };
    expect(options.subject).toBe(ids[0]);
    expect(options.parent).toBe('did:ad:drive');
    expect(options.genesisCert.signerPubkey).toEqual(
      decodeB64(await d.agent.getPublicKey()),
    );
    expect(options.genesisCert.parent).toBe('did:ad:drive');
    expect(options).not.toHaveProperty('rights');
    expect(options.genesisCert.drive).toBe('did:ad:drive');
    expect(
      d.resources.get('did:ad:drive')?.get(ai.properties.aiChatsFolder),
    ).toBe(ids[0]);
    await d.resources.get(ids[0])!.set(core.properties.name, 'Renamed');
    expect(await getOrCreateAiChatsFolder(d.typed, 'did:ad:drive')).toBe(
      ids[0],
    );
    expect(d.resources.get(ids[0])?.get(core.properties.name)).toBe('Renamed');
  }
});

it('reuses an existing valid pointer when an old signer cannot derive a subject', async () => {
  const d = device();
  const folder = await getOrCreateAiChatsFolder(d.typed, 'did:ad:drive');
  await d.resources
    .get('did:ad:drive')!
    .set(ai.properties.aiChatsFolder, folder);
  vi.spyOn(d.agent, 'aiChatsFolderSubject').mockRejectedValue(
    new Error('Sign in again'),
  );

  expect(await getOrCreateAiChatsFolder(d.typed, 'did:ad:drive')).toBe(folder);
  expect(d.store.newResource).toHaveBeenCalledTimes(1);
});

it('does not create a random replacement when the existing pointer cannot be read', async () => {
  const d = device();
  vi.spyOn(d.agent, 'aiChatsFolderSubject').mockRejectedValue(
    new Error('Sign in again'),
  );

  await expect(
    getOrCreateAiChatsFolder(d.typed, 'did:ad:drive'),
  ).rejects.toThrow('Sign in again');
  expect(d.store.newResource).not.toHaveBeenCalled();
});

it('does not resurrect a deleted deterministic folder', async () => {
  const d = device();
  d.store.isDestroyed.mockReturnValue(true);

  await expect(
    getOrCreateAiChatsFolder(d.typed, 'did:ad:drive'),
  ).rejects.toThrow('deleted');
  expect(d.store.newResource).not.toHaveBeenCalled();
});

it('does not initialize a folder over a transport failure', async () => {
  const d = device();
  const folder = await d.agent.aiChatsFolderSubject('did:ad:drive');
  d.resources.set(folder, {
    subject: folder,
    error: new AtomicError('Unavailable', ErrorType.Transport),
    get: () => undefined,
    set: async () => {},
    save: async () => {},
    hasClasses: () => false,
  });

  await expect(
    getOrCreateAiChatsFolder(d.typed, 'did:ad:drive'),
  ).rejects.toThrow('Unavailable');
  expect(d.store.newResource).not.toHaveBeenCalled();
});

it('retries a cached local drive miss against the server', async () => {
  const d = device();
  const drive = d.resources.get('did:ad:drive')!;
  drive.error = new AtomicError(
    'Resource not found locally',
    ErrorType.NotFound,
  );
  const fetchResourceFromServer = vi.fn(async () => {
    drive.error = undefined;

    return drive;
  });
  const store = { ...d.store, fetchResourceFromServer };

  const folder = await getOrCreateAiChatsFolder(
    store as unknown as Store,
    'did:ad:drive',
  );

  expect(folder).toBe(await d.agent.aiChatsFolderSubject('did:ad:drive'));
  expect(fetchResourceFromServer).toHaveBeenCalledTimes(1);
  expect(drive.get(ai.properties.aiChatsFolder)).toBe(folder);
});
