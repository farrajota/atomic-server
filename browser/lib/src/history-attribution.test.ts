import { afterEach, describe, expect, it, vi } from 'vitest';
import { Store } from './store.js';
import { testStore } from './test-store.js';
import {
  attributionForVersion,
  mergeHistoryAttributions,
  parseHistoryAttribution,
  type Attribution,
  type HistoryAttribution,
} from './history-attribution.js';

const alice = 'did:ad:agent:alice';
const bob = 'did:ad:agent:bob';

function attribution(overrides: Partial<Attribution>): Attribution {
  return {
    signer: alice,
    createdAt: 1,
    signature: 'sig',
    verified: true,
    tokens: [],
    destroy: false,
    genesis: false,
    ...overrides,
  };
}

describe('parseHistoryAttribution', () => {
  it('reads the server JSON (snake_case createdAt) and the WASM string', () => {
    const wire = {
      subject: 'did:ad:x',
      retention: 'all',
      complete: true,
      attributions: [
        {
          signer: alice,
          created_at: 42,
          signature: 'a',
          verified: true,
          tokens: [alice],
          destroy: false,
          genesis: true,
        },
        {
          signer: bob,
          created_at: 43,
          signature: 'b',
          verified: false,
          tokens: ['c-1'],
        },
      ],
    };
    const fromObject = parseHistoryAttribution(wire);
    const fromString = parseHistoryAttribution(JSON.stringify(wire));

    expect(fromObject).toEqual(fromString);
    expect(fromObject?.attributions).toHaveLength(2);
    expect(fromObject?.attributions[0]).toMatchObject({
      signer: alice,
      createdAt: 42,
      genesis: true,
      verified: true,
    });
    expect(fromObject?.attributions[1]).toMatchObject({
      signer: bob,
      verified: false,
      destroy: false,
      genesis: false,
    });
    expect(fromObject?.complete).toBe(true);
  });

  it('returns null for malformed input and skips malformed rows', () => {
    expect(parseHistoryAttribution('not json')).toBeNull();
    expect(parseHistoryAttribution({ nope: true })).toBeNull();
    const report = parseHistoryAttribution({
      attributions: [{ signer: alice }, 'junk', null],
    });
    expect(report?.attributions).toEqual([]);
    expect(report?.complete).toBe(false);
  });
});

describe('attributionForVersion', () => {
  const report: HistoryAttribution = {
    subject: 'did:ad:x',
    retention: 'all',
    complete: true,
    attributions: [
      attribution({ signature: 'g', tokens: [alice], genesis: true }),
      attribution({ signer: bob, signature: 'e', tokens: ['c-7'] }),
    ],
  };

  it('maps a version to the envelope carrying its token', () => {
    expect(attributionForVersion({ token: 'c-7' }, report)?.signer).toBe(bob);
    expect(attributionForVersion({ token: alice }, report)?.genesis).toBe(true);
    // `message` is the display field; a legacy caller may still pass it.
    expect(attributionForVersion({ message: 'c-7' }, report)?.signer).toBe(bob);
  });

  it('leaves untokened or unclaimed versions unattributed', () => {
    expect(attributionForVersion({ token: undefined }, report)).toBeUndefined();
    expect(attributionForVersion({ token: 'c-9' }, report)).toBeUndefined();
    expect(attributionForVersion({ token: 'c-7' }, null)).toBeUndefined();
  });
});

describe('mergeHistoryAttributions', () => {
  it('unions by signature, prefers verified, sorts by time', () => {
    const server: HistoryAttribution = {
      subject: 'did:ad:x',
      retention: 'latest',
      complete: false,
      attributions: [
        attribution({ signature: 'b', createdAt: 2, verified: false }),
      ],
    };
    const local: HistoryAttribution = {
      subject: 'did:ad:x',
      retention: 'all',
      complete: true,
      attributions: [
        attribution({ signature: 'a', createdAt: 1 }),
        attribution({ signature: 'b', createdAt: 2, verified: true }),
      ],
    };
    const merged = mergeHistoryAttributions(server, local);

    expect(merged?.attributions.map(a => a.signature)).toEqual(['a', 'b']);
    expect(merged?.attributions[1].verified).toBe(true);
    expect(merged?.retention).toBe('all');
    expect(merged?.complete).toBe(true);
    expect(mergeHistoryAttributions(null, local)).toBe(local);
    expect(mergeHistoryAttributions(null, null)).toBeNull();
  });
});

describe('Store history storage admission', () => {
  afterEach(() => vi.restoreAllMocks());

  const subject = 'atomic:history-owner';
  const local: HistoryAttribution = {
    subject,
    retention: 'all',
    complete: true,
    attributions: [attribution({ signature: 'local' })],
  };

  it('uses server attribution without a terminal-denied local lookup', async () => {
    const store = new Store({ serverUrl: 'https://example.com' });
    const remote = {
      ...local,
      attributions: [attribution({ signature: 'remote' })],
    };
    const fetch = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(JSON.stringify(remote)));
    const historyAttribution = vi.fn().mockResolvedValue(local);
    store.setClientDb({
      isOpen: false,
      whenOpen: async () => false,
      initError: new Error('storage denied'),
      historyAttribution,
    } as never);

    const result = await store.getHistoryAttribution(subject);

    expect(result?.attributions.map(a => a.signature)).toEqual(['remote']);
    expect(fetch).toHaveBeenCalledOnce();
    expect(historyAttribution).not.toHaveBeenCalled();
  });

  it('retains a local attribution lookup while its database opens', async () => {
    const store = new Store();
    const opening = Promise.withResolvers<boolean>();
    const db = {
      isOpen: false,
      whenOpen: () => opening.promise,
      historyAttribution: vi.fn().mockResolvedValue(local),
    };
    store.setClientDb(db as never);
    const result = store.getHistoryAttribution(subject);
    await Promise.resolve();
    expect(db.historyAttribution).not.toHaveBeenCalled();
    db.isOpen = true;
    opening.resolve(true);
    await expect(result).resolves.toEqual(local);
    expect(db.historyAttribution).toHaveBeenCalledWith(subject);
  });

  it.each(['worker', 'agent'] as const)(
    'discards retained attribution after %s changes during its read',
    async replacement => {
      const { store } = await testStore();
      vi.spyOn(globalThis, 'fetch').mockResolvedValue(
        new Response(null, { status: 404 }),
      );
      const reading = Promise.withResolvers<HistoryAttribution | null>();
      const historyAttribution = vi.fn(() => reading.promise);
      store.setClientDb({ isOpen: true, historyAttribution } as never);
      const result = store.getHistoryAttribution(subject);
      await vi.waitFor(() => expect(historyAttribution).toHaveBeenCalledOnce());

      if (replacement === 'worker')
        store.setClientDb({ isOpen: true } as never);
      else store.setAgent(undefined);

      reading.resolve(local);
      await expect(result).resolves.toBeNull();
    },
  );
});
