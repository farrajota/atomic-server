import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { Agent, decodeSecret } from '@tomic/react';

/** In-memory IndexedDB; transactions apply all writes together or none. */
const store = new Map<string, unknown>();
let holdNextTransaction = false;
let transactionStarted: (() => void) | undefined;
let pendingTransaction: { complete: () => void } | undefined;

const memoryDatabase = {
  transaction() {
    const writes = new Map<string, unknown>();
    const deletes = new Set<string>();
    let aborted = false;
    const transaction = {
      oncomplete: undefined as ((event: Event) => void) | undefined,
      onabort: undefined as ((event: Event) => void) | undefined,
      onerror: undefined as ((event: Event) => void) | undefined,
      objectStore: () => ({
        get: (key: IDBValidKey) => {
          const request = {
            result: undefined as unknown,
            onsuccess: undefined as ((event: Event) => void) | undefined,
          };
          queueMicrotask(() => {
            request.result = store.get(String(key));
            request.onsuccess?.(new Event('success'));
          });

          return request as unknown as IDBRequest;
        },
        put: (value: unknown, key: IDBValidKey) => writes.set(String(key), value),
        delete: (key: IDBValidKey) => deletes.add(String(key)),
      }),
      abort() {
        aborted = true;
        transaction.onabort?.(new Event('abort'));
      },
    };
    const complete = () => {
      if (aborted) return;
      for (const key of deletes) store.delete(key);
      for (const [key, value] of writes) store.set(key, value);
      transaction.oncomplete?.(new Event('complete'));
    };

    pendingTransaction = { complete };
    transactionStarted?.();
    transactionStarted = undefined;
    if (holdNextTransaction) holdNextTransaction = false;
    else queueMicrotask(() => queueMicrotask(complete));

    return transaction as unknown as IDBTransaction;
  },
  close() {},
};

const memoryIndexedDB = {
  open() {
    const request = {
      result: memoryDatabase,
      onsuccess: undefined as ((event: Event) => void) | undefined,
      onerror: undefined as ((event: Event) => void) | undefined,
      onblocked: undefined as ((event: Event) => void) | undefined,
    };
    queueMicrotask(() => request.onsuccess?.(new Event('success')));

    return request as unknown as IDBOpenDBRequest;
  },
};

vi.mock('idb-keyval', () => ({
  get: async (key: string) => store.get(key),
  set: async (key: string, value: unknown) => {
    store.set(key, value);
  },
  del: async (key: string) => void store.delete(key),
  keys: async () => [...store.keys()],
  update: async (key: string, updater: (value: unknown) => unknown) => {
    store.set(key, updater(store.get(key)));
  },
}));

const {
  archiveStoredAgent,
  getAgentFromIDB,
  readPreviousIdentities,
  saveAgentToIDB,
} = await import('./agentStorage');
const { waitForSessionDbKey } = await import('./localDbKey');

beforeEach(() => {
  store.clear();
  holdNextTransaction = false;
  transactionStarted = undefined;
  pendingTransaction = undefined;
  vi.stubGlobal('indexedDB', memoryIndexedDB);
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});
const AGENT_IDB_KEY = 'atomic.agent';
const AGENT_FALLBACK_KEY = 'atomic.agent.fallback';
const SESSION_KEY_PREFIX = 'atomic.clientdb.session-key.';
const WRAPPED_KEY_PREFIX = 'atomic.clientdb.wrapped-key.';
const WRAPPED_KEY_V2_PREFIX = 'atomic.clientdb.wrapped-key-v2.';

async function makeSecret(): Promise<string> {
  const keys = await Agent.generateKeyPair();

  return Agent.buildSecret(keys.privateKey, `atomic:agent:${keys.publicKey}`);
}

/** Simulates an insecure context (plain-HTTP origin), where Web Crypto is absent. */
function withoutSubtleCrypto(run: () => Promise<void>): Promise<void> {
  const real = globalThis.crypto.subtle;
  Object.defineProperty(globalThis.crypto, 'subtle', {
    value: undefined,
    configurable: true,
  });

  return run().finally(() => {
    Object.defineProperty(globalThis.crypto, 'subtle', {
      value: real,
      configurable: true,
    });
  });
}

describe('agent key storage', () => {
  beforeEach(() => store.clear());

  it('announces a sign-in before its first await, so the database opener waits for it', async () => {
    const secret = await makeSecret();
    const { subject } = decodeSecret(secret);
    const order: string[] = [];
    const started = Promise.withResolvers<void>();
    holdNextTransaction = true;
    transactionStarted = started.resolve;
    const saving = saveAgentToIDB(secret).then(() => order.push('saved'));
    const opening = waitForSessionDbKey(subject, 100).then(() =>
      order.push('opened'),
    );
    await started.promise;

    expect(order).toEqual([]);
    expect(pendingTransaction).toBeDefined();
    pendingTransaction!.complete();
    await Promise.all([saving, opening]);

    expect(order).toEqual(['saved', 'opened']);
  });

  it('never stores a readable private key where Web Crypto is available', async () => {
    await saveAgentToIDB(await makeSecret());

    expect(store.has(AGENT_IDB_KEY)).toBe(true);
    // The whole point of the non-extractable keypair: a readable copy beside it
    // would hand back exactly what non-extractability withholds.
    expect(store.has(AGENT_FALLBACK_KEY)).toBe(false);
  });

  it('discards a readable key left behind by an earlier version', async () => {
    const secret = await makeSecret();
    await saveAgentToIDB(secret);
    // An install from before the fix: plaintext sitting next to the keypair.
    store.set(AGENT_FALLBACK_KEY, {
      privateKey: 'leaked',
      subject: 'http://localhost/agents/test',
    });

    const agent = await getAgentFromIDB();

    expect(agent).toBeDefined();
    expect(store.has(AGENT_FALLBACK_KEY)).toBe(false);
  });

  it('falls back to a readable key only in an insecure context', async () => {
    const secret = await makeSecret();

    await withoutSubtleCrypto(async () => {
      await saveAgentToIDB(secret);

      expect(store.has(AGENT_FALLBACK_KEY)).toBe(true);
      expect(store.has(AGENT_IDB_KEY)).toBe(false);

      // ...and it stays usable — dropping it would lock the user out.
      const agent = await getAgentFromIDB();
      expect(agent).toBeDefined();
      expect(store.has(AGENT_FALLBACK_KEY)).toBe(true);
      expect(store.get(AGENT_FALLBACK_KEY)).toMatchObject({
        privateKey: JSON.parse(atob(secret)).privateKey,
      });
      expect(agent!.subject).toBe((await Agent.fromSecret(secret)).subject);
    });
  });

  it('signs out by removing both key records, and the session db keys', async () => {
    await saveAgentToIDB(await makeSecret());
    store.set(AGENT_FALLBACK_KEY, { privateKey: 'x', subject: 'y' });
    store.set(`${SESSION_KEY_PREFIX}fingerprint`, 'session-copy');
    store.set(`${WRAPPED_KEY_PREFIX}fingerprint`, 'wrapped-copy');
    store.set(`${WRAPPED_KEY_V2_PREFIX}fingerprint`, 'wrapped-copy-v2');

    await saveAgentToIDB(undefined);

    expect(store.has(AGENT_IDB_KEY)).toBe(false);
    expect(store.has(AGENT_FALLBACK_KEY)).toBe(false);
    // The session copy of the local-database key goes, so this signed-out
    // session can no longer open the encrypted OPFS cache...
    expect(store.has(`${SESSION_KEY_PREFIX}fingerprint`)).toBe(false);
    // ...while the wrapped copy survives, so the owning agent's cache becomes
    // readable again on their next sign-in rather than being wiped.
    expect(store.has(`${WRAPPED_KEY_PREFIX}fingerprint`)).toBe(true);
    expect(store.has(`${WRAPPED_KEY_V2_PREFIX}fingerprint`)).toBe(true);
  });
});

it('preserves the stored identity and wrapped cache keys when a save is cancelled', async () => {
  const currentSecret = await makeSecret();
  await saveAgentToIDB(currentSecret, { adoptOnDevice: false });
  const before = new Map(store);
  const controller = new AbortController();
  controller.abort();

  await expect(
    saveAgentToIDB(await makeSecret(), { signal: controller.signal }),
  ).rejects.toMatchObject({ name: 'AbortError' });

  expect([...store.entries()]).toEqual([...before.entries()]);
});

it('does not let an aborted save overwrite identity storage when it finishes late', async () => {
  const currentSecret = await makeSecret();
  const lateSecret = await makeSecret();
  await saveAgentToIDB(currentSecret, { adoptOnDevice: false });
  const before = new Map(store);
  const privateDriveStarted = Promise.withResolvers<void>();
  const privateDrive = Promise.withResolvers<string>();
  const privateDriveSpy = vi
    .spyOn(Agent, 'privateDriveSubjectFromSecret')
    .mockImplementationOnce(() => {
      privateDriveStarted.resolve();

      return privateDrive.promise;
    });
  const vaultProofSpy = vi.spyOn(Agent, 'vaultProofFromSecret');
  const controller = new AbortController();
  const saving = saveAgentToIDB(lateSecret, {
    adoptOnDevice: false,
    signal: controller.signal,
  });

  await privateDriveStarted.promise;
  controller.abort();
  await expect(saving).rejects.toMatchObject({ name: 'AbortError' });
  privateDrive.resolve('did:ad:agent:late');
  await Promise.resolve();
  expect(vaultProofSpy).not.toHaveBeenCalled();
  privateDriveSpy.mockRestore();
  vaultProofSpy.mockRestore();

  expect([...store.entries()]).toEqual([...before.entries()]);
});

it('fails a same-subject different-key sign-in without changing the identity or cache, then permits the original identity to retry', async () => {
  const original = await makeSecret();
  await saveAgentToIDB(original, { adoptOnDevice: false });
  const conflicting = JSON.parse(atob(await makeSecret()));
  conflicting.subject = JSON.parse(atob(original)).subject;
  const before = new Map(store);

  await expect(
    saveAgentToIDB(btoa(JSON.stringify(conflicting)), { adoptOnDevice: false }),
  ).rejects.toThrow(/existing local cache key.*retry/i);
  expect([...store.entries()]).toEqual([...before.entries()]);

  await saveAgentToIDB(original, { adoptOnDevice: false });
  expect((await getAgentFromIDB())?.subject).toBe(
    (await Agent.fromSecret(original)).subject,
  );
  for (const [key, value] of before) {
    if (key.startsWith('atomic.clientdb.')) expect(store.get(key)).toEqual(value);
  }
});

it('aborts a pending identity and cache-key transaction before a retry', async () => {
  await saveAgentToIDB(await makeSecret(), { adoptOnDevice: false });
  const before = new Map(store);
  const oldSecret = await makeSecret();
  const controller = new AbortController();
  const started = Promise.withResolvers<void>();
  holdNextTransaction = true;
  transactionStarted = started.resolve;
  const saving = saveAgentToIDB(oldSecret, {
    adoptOnDevice: false,
    signal: controller.signal,
  });
  await started.promise;
  const lateTransaction = pendingTransaction;

  controller.abort();
  await expect(saving).rejects.toMatchObject({ name: 'AbortError' });
  expect([...store.entries()]).toEqual([...before.entries()]);

  const retrySecret = await makeSecret();
  await saveAgentToIDB(retrySecret, { adoptOnDevice: false });
  const afterRetry = new Map(store);
  lateTransaction?.complete();

  expect([...store.entries()]).toEqual([...afterRetry.entries()]);
  expect((store.get(AGENT_IDB_KEY) as { subject: string }).subject).toBe(
    (await Agent.fromSecret(retrySecret)).subject,
  );
});

it('persists folder identities through non-extractable key restoration and keypair updates', async () => {
  const secret = await makeSecret();
  const expected = await Agent.aiChatsFoldersFromSecret(secret);
  await saveAgentToIDB(secret);
  const restored = await getAgentFromIDB();
  expect(restored?.aiChatsFolders).toEqual(expected);
  const stored = store.get(AGENT_IDB_KEY) as {
    keyPair: CryptoKeyPair;
    subject: string;
  };
  await saveAgentToIDB(stored.keyPair, stored.subject);
  expect((await getAgentFromIDB())?.aiChatsFolders).toEqual(expected);
});

describe('keeping a replaced identity', () => {
  beforeEach(() => store.clear());

  it('keeps the stored key aside, once, even after another agent is saved', async () => {
    await saveAgentToIDB(await makeSecret(), { adoptOnDevice: false });
    const { subject, keyPair } = store.get(AGENT_IDB_KEY) as {
      subject: string;
      keyPair: CryptoKeyPair;
    };

    await archiveStoredAgent(subject, ['did:ad:kept']);
    await archiveStoredAgent(subject, ['did:ad:kept', 'did:ad:other']);
    await saveAgentToIDB(await makeSecret(), { adoptOnDevice: false });

    const [previous, ...rest] = await readPreviousIdentities();
    expect(rest).toEqual([]);
    expect(previous.subject).toBe(subject);
    expect((previous.record as { keyPair: CryptoKeyPair }).keyPair).toBe(
      keyPair,
    );
    expect(previous.localOnlyDrives).toEqual(['did:ad:kept', 'did:ad:other']);
  });

  it('finds the stored key when asked for the other spelling of the same agent', async () => {
    await saveAgentToIDB(await makeSecret(), { adoptOnDevice: false });
    const { subject } = store.get(AGENT_IDB_KEY) as { subject: string };
    const key = subject.replace(/^(atomic|did:ad):agent:/, '');
    const otherSpelling = subject.startsWith('atomic:agent:')
      ? `did:ad:agent:${key}`
      : `atomic:agent:${key}`;
    expect(otherSpelling).not.toBe(subject);

    await archiveStoredAgent(otherSpelling);

    const [previous, ...rest] = await readPreviousIdentities();
    expect(rest).toEqual([]);
    expect(previous.subject).toBe(subject);
  });

  it('refuses when the device holds no key for the identity', async () => {
    await expect(archiveStoredAgent('did:ad:agent:missing')).rejects.toThrow();
  });
});
