// @vitest-environment jsdom
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { act, cleanup, renderHook } from '@testing-library/react';
import { Agent, LoroLoader, Store } from '@tomic/lib';
import type * as TomicReact from '@tomic/react';
import type * as ManagedServer from '../helpers/managedServer';
import { saveAgentToIDB } from '../helpers/agentStorage';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { useDevDrive, DEV_DRIVE_AGENT_NAME } from './useDevDrive';

const nodeRequire = createRequire(import.meta.url);

const state = vi.hoisted(() => ({
  info: vi.fn(),
  navigate: vi.fn(),
  settings: {
    setAgent: vi.fn(),
    setDrive: vi.fn(),
    setServer: vi.fn(),
  },
  realStore: undefined as Store | undefined,
  store: {
    setAgent: vi.fn(),
    waitForClientDb: vi.fn(async () => undefined),
    createDrive: vi.fn(async () => ({
      subject: `atomic:${'a'.repeat(64)}`,
    })),
  },
}));

vi.mock('@tomic/react', async original => ({
  ...(await original<typeof TomicReact>()),
  useStore: () => state.realStore ?? state.store,
}));
vi.mock('../helpers/AppSettings', () => ({
  useSettings: () => state.settings,
}));
vi.mock('../helpers/managedServer', async original => ({
  ...(await original<typeof ManagedServer>()),
  fetchManagedInfo: state.info,
}));
vi.mock('../helpers/agentStorage', () => ({
  saveAgentToIDB: vi.fn(async () => undefined),
}));
vi.mock('./useNavigateWithTransition', () => ({
  useNavigateWithTransition: () => state.navigate,
}));
vi.mock('react-hot-toast', () => ({
  default: { success: vi.fn() },
}));

const mockStore = state.store;

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(saveAgentToIDB).mockReset();
  vi.mocked(saveAgentToIDB).mockResolvedValue(undefined);
  vi.stubEnv('VITE_ATOMIC_SERVER_URL', 'https://node.example');
  window.history.replaceState(null, '', '/app/dev-drive?fixture=storage');
  localStorage.clear();
});
afterEach(() => {
  cleanup();
  state.store = mockStore;
  state.realStore = undefined;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
});

it('bootstraps a fresh synthetic identity and drive on an Open server', async () => {
  state.info.mockResolvedValue({
    version: 'test',
    acceptsNewDrives: true,
    canCreateDrives: true,
  });

  const generate = vi.spyOn(Agent, 'generateKeyPair');
  const { result } = renderHook(() => useDevDrive());

  await act(async () => result.current.createDevDrive());

  expect(generate).toHaveBeenCalledOnce();
  expect(state.info).toHaveBeenCalledWith('https://node.example');
  expect(state.store.createDrive).toHaveBeenCalledWith(
    'Dev drive',
    expect.objectContaining({ agentName: DEV_DRIVE_AGENT_NAME }),
  );
  expect(state.settings.setAgent).toHaveBeenCalledOnce();
  expect(state.settings.setDrive).toHaveBeenCalledWith(
    `atomic:${'a'.repeat(64)}`,
  );
  expect(result.current.loading).toBe(false);
});

it('keeps a real Resource save valid while final dev-drive credentials persist', async () => {
  state.info.mockResolvedValue({
    version: 'test',
    acceptsNewDrives: true,
    canCreateDrives: true,
  });

  let finishCommit!: () => void;
  let commitStarted!: () => void;
  const commitHeld = new Promise<void>(resolve => {
    finishCommit = resolve;
  });
  const commitObserved = new Promise<void>(resolve => {
    commitStarted = resolve;
  });
  let finishPersistence!: () => void;
  let persistenceStarted!: () => void;
  const persistenceHeld = new Promise<void>(resolve => {
    finishPersistence = resolve;
  });
  const persistenceObserved = new Promise<void>(resolve => {
    persistenceStarted = resolve;
  });

  vi.mocked(saveAgentToIDB).mockImplementation(async () => {
    persistenceStarted();
    await persistenceHeld;
  });

  const libEntry = nodeRequire.resolve('@tomic/lib');
  const loroWebEntry = nodeRequire.resolve('loro-crdt/web', {
    paths: [dirname(libEntry)],
  });
  const loroWasm = await readFile(
    join(dirname(loroWebEntry), 'loro_wasm_bg.wasm'),
  );
  vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
    const requestUrl =
      input instanceof URL
        ? input.href
        : typeof input === 'string'
          ? input
          : input.url;

    if (
      new URL(requestUrl, window.location.href).pathname.endsWith(
        '/loro_wasm_bg.wasm',
      )
    ) {
      return new Response(loroWasm, {
        headers: { 'Content-Type': 'application/wasm' },
      });
    }

    throw new Error('network disabled');
  });
  await LoroLoader.initializeLoro();
  vi.stubGlobal('fetch', async () => {
    throw new Error('network disabled');
  });
  const store = new Store({
    serverUrl: 'https://node.example',
    connect: false,
  });
  state.realStore = store;
  vi.spyOn(store, 'waitForClientDb').mockResolvedValue(false);
  store.injectFetch(async () => {
    throw new Error('network disabled');
  });
  store.setServerConnected(true);

  type CommitFixture = {
    signature: string;
    signer: string;
    id?: string;
    [key: string]: unknown;
  };
  const storeWithClient = store as unknown as {
    client: {
      postCommit: (commit: CommitFixture) => Promise<CommitFixture>;
    };
  };
  let commitCount = 0;
  const postedSigners: string[] = [];

  storeWithClient.client.postCommit = async commit => {
    commitCount++;
    postedSigners.push(commit.signer);

    if (commitCount === 2) {
      commitStarted();
      await commitHeld;
    }

    return {
      ...commit,
      id: `https://node.example/commits/${commit.signature}`,
    };
  };

  let saveResult: Promise<unknown> | undefined;
  vi.spyOn(store, 'createDrive').mockImplementation(async () => {
    const resource = await store.newResource({
      deferGenesis: true,
      noParent: true,
      isA: 'https://atomicdata.dev/classes/Folder',
    });
    await resource.set(
      'https://atomicdata.dev/properties/name',
      'Before bootstrap finalization',
      false,
    );
    await resource.save();
    await resource.set(
      'https://atomicdata.dev/properties/name',
      'Save during bootstrap',
      false,
    );
    saveResult = resource.save().then(
      result => ({ result }),
      error => ({ error }),
    );

    return resource;
  });

  const { result } = renderHook(() => useDevDrive());
  let bootstrapOutcome:
    | Promise<{ ok: true } | { ok: false; error: unknown }>
    | undefined;

  try {
    await act(async () => {
      bootstrapOutcome = result.current.createDevDrive().then(
        () => ({ ok: true as const }),
        error => ({ ok: false as const, error }),
      );
      const bootstrapFinished = bootstrapOutcome.then(outcome => {
        if (!outcome.ok) throw outcome.error;
        throw new Error('Bootstrap completed before the controlled save gates');
      });
      await Promise.race([
        Promise.all([commitObserved, persistenceObserved]),
        bootstrapFinished,
      ]);
    });

    if (!saveResult || !bootstrapOutcome) {
      throw new Error(
        'The real Resource save did not start during drive creation',
      );
    }

    const bootstrapCompleted = bootstrapOutcome;
    await act(async () => {
      finishPersistence();
      const outcome = await bootstrapCompleted;
      if (!outcome.ok) throw outcome.error;
    });
    finishCommit();

    const outcome = await saveResult;
    expect(outcome).toHaveProperty('result');
    expect(outcome).not.toHaveProperty('error');
    expect(postedSigners[0]).toBeDefined();
    expect(postedSigners.every(signer => signer === postedSigners[0])).toBe(
      true,
    );
  } finally {
    finishPersistence();
    finishCommit();
    if (bootstrapOutcome) await bootstrapOutcome;
    if (saveResult) await saveResult;
  }
});

it.each([
  [
    'owner host',
    { version: 'test', acceptsNewDrives: false, canCreateDrives: false },
  ],
  [
    'owner host with an authorized existing owner',
    { version: 'test', acceptsNewDrives: false, canCreateDrives: true },
  ],
  ['non-node or failed metadata', { acceptsNewDrives: true }],
])(
  'offers existing-identity sign-in on a %s without generating keys',
  async (_name, info) => {
    state.info.mockResolvedValue(info);

    const generate = vi.spyOn(Agent, 'generateKeyPair');
    const { result } = renderHook(() => useDevDrive());

    await act(async () => result.current.createDevDrive());

    expect(generate).not.toHaveBeenCalled();
    expect(state.store.setAgent).not.toHaveBeenCalled();
    expect(state.store.createDrive).not.toHaveBeenCalled();
    expect(state.settings.setAgent).not.toHaveBeenCalled();
    expect(state.settings.setDrive).not.toHaveBeenCalled();
    expect(state.settings.setServer).not.toHaveBeenCalled();

    const destination = new URL(
      state.navigate.mock.calls[0][0].to,
      window.location.origin,
    );

    expect(destination.pathname).toBe('/app/welcome');
    expect(destination.searchParams.get('step')).toBe('signin');
    expect(destination.searchParams.get('next')).toBe(
      '/app/dev-drive?fixture=storage',
    );
    expect(state.navigate.mock.calls[0][0].replace).toBe(true);
    expect(result.current.loading).toBe(false);
  },
);
