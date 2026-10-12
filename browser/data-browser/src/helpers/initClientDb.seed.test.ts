import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type { Store } from '@tomic/lib';

const state = vi.hoisted(() => ({
  batches: [] as string[][],
  withSnapshot: [] as { subject: string; jsonAd: string; snapshot: unknown }[],
  workers: [] as {
    ready: PromiseWithResolvers<void>;
    open: PromiseWithResolvers<boolean>;
    destroyed: boolean;
    initError: Error | undefined;
    unsupportedEnvironment: boolean;
  }[],
  calls: [] as { kind: string; subjects?: string[] }[],
  fingerprints: new Map<string, string>(),
  failWrites: false,
}));

vi.mock('@tomic/lib', () => ({
  StoreEvents: { AgentChanged: 'agent' },
  perfSpan: () => () => {},
  ClientDbWorker: class {
    ready = Promise.withResolvers<void>();
    open = Promise.withResolvers<boolean>();
    destroyed = false;
    initError: Error | undefined;
    unsupportedEnvironment = false;
    constructor() {
      state.workers.push(this);
    }
    init() {
      return this.ready.promise;
    }
    whenOpen() {
      return this.open.promise;
    }
    setSeedPromise() {}
    waitForReady() {
      return this.ready.promise.then(() => true);
    }
    flush() {
      return Promise.resolve();
    }
    allSubjects() {
      state.calls.push({ kind: 'allSubjects' });

      return Promise.resolve([]);
    }
    putResources(resources: string[]) {
      state.batches.push(resources);
      state.calls.push({ kind: 'putResources', subjects: resources });
      if (state.failWrites)
        return Promise.reject(new Error('seed write failed'));

      return Promise.resolve();
    }
    putResourceWithSnapshot(
      subject: string,
      jsonAd: string,
      snapshot: unknown,
    ) {
      state.withSnapshot.push({ subject, jsonAd, snapshot });
      state.calls.push({
        kind: 'putResourceWithSnapshot',
        subjects: [subject],
      });

      return Promise.resolve();
    }
    destroy() {
      this.destroyed = true;
      this.open.resolve(false);
    }
  },
}));
vi.mock('@tomic/lib/client-db.worker.js?url', () => ({ default: 'worker.js' }));
vi.mock('./wasmUrls', () => ({
  wasmJsUrl: () => 'wasm.js',
  compiledAtomicWasm: async () => undefined,
}));
vi.mock('./localDbKey', () => ({
  agentDbFingerprint: async () => 'agent',
  getSessionDbKey: async () => new Uint8Array(32),
  hasWrappedDbKey: async () => false,
  getOrCreateSessionDbKey: async () => new Uint8Array(32),
}));

beforeEach(() => {
  // A static import would share module-local first-start/worker state across scenarios.
  vi.resetModules();
  state.batches.splice(0);
  state.withSnapshot.splice(0);
  state.workers.splice(0);
  state.calls.splice(0);
  state.fingerprints.clear();
  state.failWrites = false;
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => state.fingerprints.get(key) ?? null,
    setItem: (key: string, value: string) => state.fingerprints.set(key, value),
  });
  vi.stubGlobal('window', { location: { pathname: '/app' } });
  vi.stubGlobal('Worker', class {});
});

afterEach(() => vi.unstubAllGlobals());

const IS_A = 'https://atomicdata.dev/properties/isA';

function fakeResource(
  subject: string,
  props: Record<string, unknown>,
  history?: Uint8Array,
) {
  return {
    subject,
    loading: false,
    new: false,
    hasPendingCommits: false,
    get: (prop: string) => props[prop],
    getEntries: () => Object.entries(props),
    hasLoroDoc: () => !!history,
    getLoroDoc: () =>
      history && {
        oplogVersion: () => ({ toJSON: () => new Map([['7', 24]]) }),
        export: () => history,
      },
  };
}

function makeStore(
  resources: Map<string, unknown>,
  initialAgent?: { subject: string },
) {
  let agent = initialAgent;
  let onAgentChanged:
    | ((next: { subject: string } | undefined) => void)
    | undefined;
  const store = {
    expectClientDb() {},
    getAgent: () => agent,
    on: (_: string, callback: typeof onAgentChanged) => {
      onAgentChanged = callback;

      return () => {};
    },
    getServerUrl: () => 'http://localhost',
    resources,
    setClientDb: vi.fn(),
    notifyError: vi.fn(),
    changeAgent(next: { subject: string } | undefined) {
      agent = next;
      onAgentChanged?.(next);
    },
  };

  return store;
}

async function flushMicrotasks() {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

it('seeds a resource with Loro history using its snapshot', async () => {
  const history = new Uint8Array([1, 2, 3]);
  const ontology = fakeResource(
    'atomic:ontology',
    { [IS_A]: ['https://atomicdata.dev/class/ontology'], name: 'o' },
    history,
  );
  const property = fakeResource('https://example.com/p', {
    [IS_A]: ['https://atomicdata.dev/classes/Property'],
    shortname: 'p',
  });
  const store = makeStore(
    new Map<string, unknown>([
      [ontology.subject, ontology],
      [property.subject, property],
    ]),
  );
  const { initClientDb } = await import('./initClientDb');
  initClientDb(store as unknown as Store);
  await vi.waitFor(() => expect(state.workers).toHaveLength(1));
  state.workers[0].ready.resolve();
  state.workers[0].open.resolve(true);

  await vi.waitFor(() => expect(state.withSnapshot).toHaveLength(1));
  expect(state.withSnapshot[0].subject).toBe('atomic:ontology');
  expect(state.withSnapshot[0].snapshot).toBe(history);
  expect(JSON.parse(state.withSnapshot[0].jsonAd)['@id']).toBe(
    'atomic:ontology',
  );
  expect(state.batches.flat().map(json => JSON.parse(json)['@id'])).toEqual([
    'https://example.com/p',
  ]);
});

it.each([
  { reason: 'unsupported', unsupported: true },
  { reason: 'denied', unsupported: false },
])(
  'does not inspect or seed a database when initialization is $reason',
  async ({ unsupported }) => {
    const property = fakeResource('https://example.com/p', {
      [IS_A]: ['https://atomicdata.dev/classes/Property'],
      shortname: 'p',
    });
    const store = makeStore(new Map([[property.subject, property]]));
    const { initClientDb } = await import('./initClientDb');
    initClientDb(store as unknown as Store);
    await vi.waitFor(() => expect(state.workers).toHaveLength(1));
    state.workers[0].unsupportedEnvironment = unsupported;
    state.workers[0].initError = new Error(
      `storage ${unsupported ? 'unsupported' : 'denied'}`,
    );
    state.workers[0].ready.resolve();
    state.workers[0].open.resolve(false);
    await flushMicrotasks();

    expect(state.calls).toEqual([]);
    expect(state.fingerprints.size).toBe(0);
  },
);

it('waits for the real open and seeds properties before other first-start resources', async () => {
  const property = fakeResource('https://example.com/p', {
    [IS_A]: ['https://atomicdata.dev/classes/Property'],
    shortname: 'p',
  });
  const resource = fakeResource('https://example.com/r', { name: 'resource' });
  const store = makeStore(
    new Map([
      [property.subject, property],
      [resource.subject, resource],
    ]),
  );
  const { initClientDb } = await import('./initClientDb');
  initClientDb(store as unknown as Store);
  await vi.waitFor(() => expect(state.workers).toHaveLength(1));
  state.workers[0].ready.resolve();
  await flushMicrotasks();
  expect(state.calls).toEqual([]);

  state.workers[0].open.resolve(true);
  await vi.waitFor(() => expect(state.fingerprints.size).toBe(1));
  expect(state.calls.map(call => call.kind)).toEqual([
    'allSubjects',
    'putResources',
    'putResources',
  ]);
  expect(
    state.calls
      .slice(1)
      .map(call => call.subjects?.map(json => JSON.parse(json)['@id'])),
  ).toEqual([['https://example.com/p'], ['https://example.com/r']]);
});

it('does not advance the bootstrap fingerprint when a required seed write fails', async () => {
  const property = fakeResource('https://example.com/p', {
    [IS_A]: ['https://atomicdata.dev/classes/Property'],
    shortname: 'p',
  });
  const store = makeStore(new Map([[property.subject, property]]));
  const { initClientDb } = await import('./initClientDb');
  initClientDb(store as unknown as Store);
  await vi.waitFor(() => expect(state.workers).toHaveLength(1));
  state.failWrites = true;
  state.workers[0].ready.resolve();
  state.workers[0].open.resolve(true);
  await vi.waitFor(() =>
    expect(state.calls.some(call => call.kind === 'putResources')).toBe(true),
  );
  await flushMicrotasks();

  expect(state.fingerprints.size).toBe(0);
});

it('does not seed the prior identity after its delayed open loses ownership', async () => {
  const property = fakeResource('https://example.com/old-property', {
    [IS_A]: ['https://atomicdata.dev/classes/Property'],
    shortname: 'p',
  });
  const store = makeStore(new Map([[property.subject, property]]), {
    subject: 'did:ad:old',
  });
  const { initClientDb } = await import('./initClientDb');
  initClientDb(store as unknown as Store);
  await vi.waitFor(() => expect(state.workers).toHaveLength(1));
  const oldWorker = state.workers[0];
  oldWorker.ready.resolve();
  await flushMicrotasks();

  store.changeAgent({ subject: 'did:ad:new' });
  oldWorker.open.resolve(true);
  await vi.waitFor(() => expect(state.workers).toHaveLength(2));
  const replacement = state.workers[1];
  replacement.ready.resolve();
  replacement.open.resolve(true);
  await flushMicrotasks();

  expect(state.calls).toEqual([]);
  expect(state.fingerprints.size).toBe(0);
});
