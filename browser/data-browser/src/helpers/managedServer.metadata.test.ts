import { signRequest, type Agent } from '@tomic/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { EMPTY_NODE_INFO, fetchManagedInfo } from './managedServer';
import { serverProps } from './serverOntology';

vi.mock('@tomic/react', () => ({
  signRequest: vi.fn(
    async (url: string, _agent: Agent, headers: HeadersInit) => ({
      ...Object.fromEntries(new Headers(headers).entries()),
      'x-atomic-signed-url': url,
    }),
  ),
}));

const SERVER_URL = 'https://node.example';
const AGENT = { subject: 'did:ad:agent:synthetic-caller' } as Agent;

function stubFetch(body: unknown, options: { ok?: boolean } = {}) {
  const fetchMock = vi.fn<typeof fetch>(
    async (_input, _init) =>
      new Response(JSON.stringify(body), {
        status: options.ok === false ? 403 : 200,
      }),
  );
  vi.stubGlobal('fetch', fetchMock);

  return fetchMock;
}

describe('fetchManagedInfo caller-specific creation metadata', () => {
  beforeEach(() => {
    vi.mocked(signRequest).mockReset();
    vi.mocked(signRequest).mockImplementation(async (url, _agent, headers) => ({
      ...Object.fromEntries(new Headers(headers).entries()),
      'x-atomic-signed-url': url,
    }));
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it.each([true, false])(
    'returns signed caller canCreateDrives=%s',
    async canCreateDrives => {
      const fetchMock = stubFetch({
        [serverProps.acceptsNewDrives]: true,
        [serverProps.canCreateDrives]: canCreateDrives,
      });

      const info = await fetchManagedInfo(SERVER_URL, AGENT);

      const fetchCall = fetchMock.mock.calls[0];
      if (!fetchCall) throw new Error('Expected metadata fetch call');
      const [requestUrl, requestInit] = fetchCall;
      if (!requestInit) throw new Error('Expected metadata request init');
      const [signedUrl, signedAgent, signingHeaders] =
        vi.mocked(signRequest).mock.calls[0];
      expect(info.canCreateDrives).toBe(canCreateDrives);
      expect(signedUrl).toBe(`${SERVER_URL}/server`);
      expect(signedAgent).toBe(AGENT);
      expect(new Headers(signingHeaders).get('accept')).toBe(
        'application/ad+json',
      );
      expect(requestUrl).toBe(signedUrl);
      expect(new Headers(requestInit.headers).get('x-atomic-signed-url')).toBe(
        requestUrl,
      );
      expect(new Headers(requestInit.headers).get('accept')).toBe(
        'application/ad+json',
      );
      expect(requestInit.credentials).toBe('omit');
    },
  );

  it.each([
    ['absent caller field, globally open', undefined, true],
    ['malformed caller field, globally open', 'true', true],
    ['absent caller field, globally closed', undefined, false],
    ['malformed caller field, globally closed', 1, false],
  ])(
    '%s falls back to global admission',
    async (_case, callerValue, acceptsNewDrives) => {
      const body: Record<string, unknown> = {
        [serverProps.acceptsNewDrives]: acceptsNewDrives,
      };
      if (callerValue !== undefined)
        body[serverProps.canCreateDrives] = callerValue;
      stubFetch(body);

      const info = await fetchManagedInfo(SERVER_URL, AGENT);

      expect(info.canCreateDrives).toBe(acceptsNewDrives);
    },
  );

  it('denies explicit capability when a signed metadata request is refused', async () => {
    stubFetch({}, { ok: false });

    const info = await fetchManagedInfo(SERVER_URL, AGENT);

    expect(info.canCreateDrives).toBe(false);
  });

  it('denies explicit capability when signing fails without making a public request', async () => {
    vi.mocked(signRequest).mockRejectedValueOnce(new Error('signing failed'));
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);

    const info = await fetchManagedInfo(SERVER_URL, AGENT);

    expect(info.canCreateDrives).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('denies explicit capability when the signed request has a network failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockRejectedValue(new Error('network failure')),
    );

    const info = await fetchManagedInfo(SERVER_URL, AGENT);

    expect(info.canCreateDrives).toBe(false);
  });

  it('keeps unsigned metadata public, cookie-free, and on legacy defaults', async () => {
    const fetchMock = stubFetch({});

    const info = await fetchManagedInfo(SERVER_URL);

    expect(info).toMatchObject({
      ...EMPTY_NODE_INFO,
      acceptsNewDrives: true,
    });
    expect(info.canCreateDrives).toBeUndefined();
    expect(signRequest).not.toHaveBeenCalled();
    expect(fetchMock).toHaveBeenCalledWith(`${SERVER_URL}/server`, {
      headers: { Accept: 'application/ad+json' },
      credentials: 'omit',
    });
  });
});
