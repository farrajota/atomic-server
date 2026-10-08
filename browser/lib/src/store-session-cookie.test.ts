import { afterEach, beforeEach, describe, it, vi } from 'vitest';
import { Agent } from './agent.js';
import { JSCryptoProvider } from './CryptoProvider.js';
import { AUTH_PROOF_MAX_AGE_MS } from './authentication.js';
import { Store } from './store.js';

const SERVER = 'https://atomic.example';
const AUTH_TIMESTAMP = 'https://atomicdata.dev/properties/auth/timestamp';

/** The page's cookies, as `document.cookie` exposes and accepts them. */
let jar: Map<string, string>;
let documentEvents: EventTarget;

/**
 * The browser surface the session cookie touches: `window` (the global, as in
 * a page), `location`, `document.cookie` and focus/visibility events. The lib
 * tests run in Node, which has none of these.
 */
function installPage(origin: string) {
  const url = new URL(origin);
  const windowEvents = new EventTarget();
  jar = new Map();
  documentEvents = new EventTarget();
  const storage = new Map<string, string>();

  vi.stubGlobal('window', globalThis);
  vi.stubGlobal('location', {
    origin: url.origin,
    protocol: url.protocol,
    hostname: url.hostname,
    href: url.href,
  });
  vi.stubGlobal(
    'addEventListener',
    windowEvents.addEventListener.bind(windowEvents),
  );
  vi.stubGlobal(
    'removeEventListener',
    windowEvents.removeEventListener.bind(windowEvents),
  );
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key),
  });
  vi.stubGlobal('document', {
    visibilityState: 'visible',
    get cookie() {
      return [...jar].map(([name, value]) => `${name}=${value}`).join('; ');
    },
    set cookie(written: string) {
      const [pair, ...attributes] = written.split(';');
      const separator = pair.indexOf('=');
      const name = pair.slice(0, separator).trim();
      const expired = attributes.some(a => /^\s*Max-Age=-/i.test(a));

      if (expired) jar.delete(name);
      else jar.set(name, pair.slice(separator + 1));
    },
    addEventListener: documentEvents.addEventListener.bind(documentEvents),
    removeEventListener:
      documentEvents.removeEventListener.bind(documentEvents),
  });
  // Signing in also reaches for the server; nothing here is under test.
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => new Response('', { status: 404 })),
  );
}

/** When the proof in the session cookie was signed, if there is one. */
function cookieSignedAt(): number | undefined {
  const value = jar.get('atomic_session');

  if (!value) return undefined;

  return JSON.parse(atob(decodeURIComponent(value)))[AUTH_TIMESTAMP];
}

/** The agent the session cookie's proof speaks for, if there is one. */
function cookieAgent(): string | undefined {
  const value = jar.get('atomic_session');

  if (!value) return undefined;

  return JSON.parse(atob(decodeURIComponent(value)))[
    'https://atomicdata.dev/properties/auth/agent'
  ];
}

async function newAgent(): Promise<Agent> {
  const keys = await Agent.generateKeyPair();

  return new Agent(
    new JSCryptoProvider(keys.privateKey),
    `did:ad:agent:${keys.publicKey}`,
  );
}

beforeEach(() => {
  vi.useFakeTimers({ now: new Date('2026-10-06T12:00:00Z') });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe('Store session cookie', () => {
  it('keeps a same-origin session cookie from ever ageing out', async ({
    expect,
  }) => {
    installPage(SERVER);
    const store = new Store({ serverUrl: SERVER, connect: false });
    store.setAgent(await newAgent());
    await vi.advanceTimersByTimeAsync(0);

    // Only WebSocket traffic for 20 minutes: nothing else renews the cookie.
    for (let elapsed = 0; elapsed <= 20 * 60_000; elapsed += 10_000) {
      const signedAt = cookieSignedAt();
      expect(signedAt).toBeDefined();
      expect(Date.now() - signedAt!).toBeLessThan(AUTH_PROOF_MAX_AGE_MS);
      await vi.advanceTimersByTimeAsync(10_000);
    }

    store.stopSessionCookieRefresh();
  });

  it('refreshes a stale cookie as soon as the tab is visible again', async ({
    expect,
  }) => {
    installPage(SERVER);
    const store = new Store({ serverUrl: SERVER, connect: false });
    store.setAgent(await newAgent());
    await vi.advanceTimersByTimeAsync(0);
    const firstProof = cookieSignedAt()!;

    // A hidden tab's timers are throttled: time passes without them firing.
    vi.setSystemTime(Date.now() + 3 * 60_000);
    documentEvents.dispatchEvent(new Event('visibilitychange'));
    await vi.advanceTimersByTimeAsync(0);

    expect(cookieSignedAt()).toBe(Date.now());
    expect(cookieSignedAt()).toBeGreaterThan(firstProof);
    store.stopSessionCookieRefresh();
  });

  it('stops refreshing once the agent signs out', async ({ expect }) => {
    installPage(SERVER);
    const store = new Store({ serverUrl: SERVER, connect: false });
    store.setAgent(await newAgent());
    await vi.advanceTimersByTimeAsync(0);

    store.setAgent(undefined);
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    documentEvents.dispatchEvent(new Event('visibilitychange'));
    await vi.advanceTimersByTimeAsync(0);

    expect(cookieSignedAt()).toBeUndefined();
  });

  it('does not reinstall a signed-out agent when a refresh finishes signing after sign-out', async ({
    expect,
  }) => {
    installPage(SERVER);
    const store = new Store({ serverUrl: SERVER, connect: false });
    store.setAgent(await newAgent());
    await vi.advanceTimersByTimeAsync(0);

    // A stale proof makes the wake-up start a refresh, which signs
    // asynchronously; the user signs out before that signature is ready.
    vi.setSystemTime(Date.now() + 3 * 60_000);
    documentEvents.dispatchEvent(new Event('visibilitychange'));
    store.setAgent(undefined);
    await vi.advanceTimersByTimeAsync(0);

    expect(cookieSignedAt()).toBeUndefined();
  });

  it('does not install the previous agent when the agent changes during sign-in', async ({
    expect,
  }) => {
    installPage(SERVER);
    const store = new Store({ serverUrl: SERVER, connect: false });
    const first = await newAgent();
    const second = await newAgent();

    store.setAgent(first);
    store.setAgent(second);
    await vi.advanceTimersByTimeAsync(0);

    expect(cookieAgent()).toBe(second.subject);
  });

  it('stops refreshing when told the store is being discarded', async ({
    expect,
  }) => {
    installPage(SERVER);
    const store = new Store({ serverUrl: SERVER, connect: false });
    store.setAgent(await newAgent());
    await vi.advanceTimersByTimeAsync(0);
    const proof = cookieSignedAt();

    store.stopSessionCookieRefresh();
    await vi.advanceTimersByTimeAsync(10 * 60_000);

    expect(cookieSignedAt()).toBe(proof);
  });

  it('leaves the cookie alone for a server on another origin', async ({
    expect,
  }) => {
    installPage('https://app.example');
    const store = new Store({ serverUrl: SERVER, connect: false });
    store.setAgent(await newAgent());
    await vi.advanceTimersByTimeAsync(0);
    const proof = cookieSignedAt();

    await vi.advanceTimersByTimeAsync(10 * 60_000);

    expect(cookieSignedAt()).toBe(proof);
  });
});
