/**
 * Opt-in real-server matrix (run from browser/e2e):
 *   ATOMIC_NAV_FIXTURE=/fixture-state/navigation-manifest.json ATOMIC_NAV_IDENTITIES=/fixture-state/identities.json pnpm exec playwright test --config=./playwright.config.ts tests/owner-mode-navigation.spec.ts
 * Both files are required at runner startup: the manifest is the public v1 seed
 * output. The identities JSON is version 1 and has owner, readonly, and outsider
 * entries under `identities`; each has `subject` and `secret`. The synthetic
 * private secrets are consumed only in memory and never attached or logged.
 * The seeded owner-mode server must already run at its manifest origin and
 * alternate *.localhost hostname. Do not use session-fixtures' before(): it
 * creates a dev drive that owner mode refuses. The suite uses the real app,
 * SDK, server, and worker, with no app/server response mocks. It uses base
 * Playwright instead of the auto failure-state fixture because that fixture
 * records raw page URLs. Trace, automatic screenshots, and video are disabled;
 * manual screenshots mask editable fields and diagnostics redact secrets.
 * Checks cover browser-visible behavior and transport; refusal observation is
 * 35 seconds, and OPFS behavior is simulated in a worker, not Firefox private mode.
 */
// The matrix keeps fresh no-home and adopted shared-drive recovery distinct.
// Protected-read warnings require real anonymous SUB/SYNC request/error pairs;
// neither raw Unauthorized UI nor ongoing denied-read loops is acceptable UX.
import { readFileSync } from 'node:fs';
import {
  test,
  expect,
  type BrowserContext,
  type Page,
  type TestInfo,
} from '@playwright/test';
import {
  installAbsentLocalOllama,
  installEmptyDiscoveryRoom,
  installStubbedWebSearchMcp,
} from './fixtures';

type Role = 'owner' | 'readonly' | 'outsider';
type Viewport = 'desktop' | 'mobile';
type Manifest = {
  origin: string;
  identities: Record<Role, string>;
  resources: Record<string, string>;
  routes: Record<string, string>;
};
type Identities = {
  version: 1;
  identities: Record<Role, { subject: string; secret: string }>;
};
type ScenarioDiagnostic = {
  kind: string;
  message: string;
  url?: string;
  status?: number;
  expected?: boolean;
  operation?: 'SUB' | 'SYNC';
  resource?: string;
  principal?: string | null;
  socket?: number;
  document?: number;
};

const manifestPath = process.env.ATOMIC_NAV_FIXTURE;
const identitiesPath = process.env.ATOMIC_NAV_IDENTITIES;

if (Boolean(manifestPath) !== Boolean(identitiesPath)) {
  throw new Error(
    'Owner-mode navigation requires both ATOMIC_NAV_FIXTURE and ATOMIC_NAV_IDENTITIES; set both to the seeded fixture files.',
  );
}

test.use({ trace: 'off', screenshot: 'off', video: 'off' });
const isolatedContexts = new Set<BrowserContext>();

async function installExternalFixtures(context: BrowserContext) {
  await Promise.all([
    installEmptyDiscoveryRoom(context),
    installAbsentLocalOllama(context),
    installStubbedWebSearchMcp(context),
  ]);
}

// This opt-in suite is intentionally absent from the generic Playwright run.
// Read these two runtime inputs once during module initialization; never attach
// or log their contents (the identities file contains private synthetic secrets).
const manifest: Manifest | undefined = manifestPath
  ? JSON.parse(readFileSync(manifestPath, 'utf8'))
  : undefined;
const identities: Identities | undefined = identitiesPath
  ? JSON.parse(readFileSync(identitiesPath, 'utf8'))
  : undefined;

if (manifest && identities && identities.version !== 1) {
  throw new Error('ATOMIC_NAV_IDENTITIES must use version 1.');
}

const routes = [
  { name: 'root', key: 'root' },
  { name: 'app', key: 'app' },
  { name: 'newResource', key: 'newResource' },
  { name: 'privateResource', key: 'privateResource' },
  { name: 'publicResource', key: 'publicResource' },
  { name: 'settings', key: 'settings' },
  { name: 'invite', key: 'invite' },
  { name: 'table', key: 'table' },
] as const;
const viewports: Record<Viewport, { width: number; height: number }> = {
  desktop: { width: 1200, height: 800 },
  mobile: { width: 412, height: 915 },
};

function redact(value: string) {
  let result = value;
  const inviteToken = manifest?.routes.invite
    ? new URL(manifest.routes.invite).searchParams.get('token')
    : null;
  const privateValues = [
    ...(identities
      ? Object.values(identities.identities).map(identity => identity.secret)
      : []),
    ...(inviteToken
      ? [
          inviteToken,
          encodeURIComponent(inviteToken),
          encodeURIComponent(encodeURIComponent(inviteToken)),
        ]
      : []),
  ];
  for (const secret of privateValues)
    result = result.split(secret).join('[redacted]');

  try {
    const url = new URL(result);

    for (const key of ['token', 'secret', 'inviteToken']) {
      if (url.searchParams.has(key)) url.searchParams.set(key, '[redacted]');
    }

    const next = url.searchParams.get('next');

    if (next) {
      const nested = new URL(next, url.origin);
      for (const key of ['token', 'secret', 'inviteToken'])
        nested.searchParams.delete(key);
      url.searchParams.set(
        'next',
        `${nested.pathname}${nested.search}${nested.hash}`,
      );
    }

    result = url.toString();
  } catch {
    // Console messages need not be URLs.
  }

  return result;
}

async function safeNavigation(action: () => Promise<unknown>) {
  try {
    await action();
  } catch (error) {
    throw new Error(
      redact(error instanceof Error ? error.message : String(error)),
    );
  }
}

async function fillSecret(page: Page, secret: string) {
  try {
    await page.getByLabel('Agent secret').fill(secret);
  } catch (error) {
    throw new Error(
      redact(error instanceof Error ? error.message : String(error)),
    );
  }
}

async function openSignIn(page: Page) {
  const field = page.getByLabel('Agent secret');
  const signInButton = page.getByRole('button', {
    name: 'Sign in',
    exact: true,
  });
  await expect
    .poll(
      async () =>
        (await field.isVisible().catch(() => false)) ||
        (await signInButton.isVisible().catch(() => false)),
      { timeout: 30_000 },
    )
    .toBe(true);
  if (!(await field.isVisible().catch(() => false))) await signInButton.click();
  await expect(field).toBeVisible({ timeout: 30_000 });
  await expect(field).toBeEnabled();
  await field.click();
}

type CommitAttempts = {
  httpPosts: string[];
  websocketCommits: number;
  websocketErrors: number;
};

function observeCommitAttempts(page: Page): CommitAttempts {
  const attempts: CommitAttempts = {
    httpPosts: [],
    websocketCommits: 0,
    websocketErrors: 0,
  };
  page.on('request', request => {
    if (request.method() === 'POST' && /commit/i.test(request.url())) {
      attempts.httpPosts.push(request.url());
    }
  });
  page.on('websocket', socket => {
    socket.on('framesent', frame => {
      const payload = frame.payload;

      if (
        typeof payload !== 'string' &&
        payload.byteLength > 0 &&
        payload[0] === 0x13
      ) {
        attempts.websocketCommits += 1;
      }
    });
    socket.on('framereceived', frame => {
      const payload = frame.payload;

      if (
        typeof payload !== 'string' &&
        payload.byteLength > 0 &&
        payload[0] === 0x03
      ) {
        attempts.websocketErrors += 1;
      }
    });
  });

  return attempts;
}

function expectNoCommitAttempts(attempts: CommitAttempts) {
  expect(attempts.httpPosts).toHaveLength(0);
  expect(attempts.websocketCommits).toBe(0);
}

async function expectStoreReady(page: Page) {
  await expect
    .poll(() => page.evaluate(() => typeof window.store !== 'undefined'))
    .toBe(true);
}

async function expectIdentityUnlocked(page: Page, role: Role) {
  const identity = identities?.identities[role];
  if (!identity) throw new Error(`Missing runtime identity for ${role}.`);
  await expect
    .poll(() => page.evaluate(() => window.store.getAgent()?.subject ?? null), {
      timeout: 30_000,
    })
    .toBe(identity.subject);
}

function routeUrl(key: string, host?: string) {
  if (!manifest)
    throw new Error('The owner-mode navigation fixture is not enabled.');
  const url = new URL(manifest.routes[key]);
  if (host) url.hostname = host;

  return url.toString();
}

async function signIn(page: Page, role: Role) {
  const secret = identities?.identities[role].secret;
  if (!secret) throw new Error(`Missing runtime identity for ${role}.`);
  await safeNavigation(() => page.goto(routeUrl('root')));
  await expectStoreReady(page);
  await openSignIn(page);
  await fillSecret(page, secret);
  await expectIdentityUnlocked(page, role);
}

async function visitAs(
  page: Page,
  destination: string,
  role: Exclude<Role, 'outsider'> | 'anonymous',
) {
  if (role === 'owner' || role === 'readonly') await signIn(page, role);
  await safeNavigation(() => page.goto(destination));
}

// Read the established session before navigating to root. A readable deep link
// adopts its genesis-stamped drive; that is not personal-drive enrollment.
async function readonlySessionDrive(page: Page) {
  await expectIdentityUnlocked(page, 'readonly');
  const subject = new URL(page.url()).searchParams.get('subject');

  if (subject) {
    const resourceState = await page.evaluate(async openedSubject => {
      const resource = await window.store.getResource(openedSubject);

      return {
        error: resource.error?.message ?? null,
        drive:
          resource.get('https://atomicdata.dev/properties/drive') ??
          (resource
            .getClasses()
            .includes('https://atomicdata.dev/classes/Drive')
            ? resource.subject
            : null),
      };
    }, subject);
    expect(resourceState.error).toBeNull();
    expect(resourceState.drive).toBe(manifest!.resources.drive);
    await expect
      .poll(() => page.evaluate(() => window.store.getDrive() || null))
      .toBe(resourceState.drive);
  }

  const drive = await page.evaluate(() => window.store.getDrive() || null);

  if (drive !== null && drive !== manifest!.resources.drive) {
    // Selecting the key-derived recovery target is not a genesis commit.
    // Reject every other drive, and require an actual absent resource rather
    // than treating any unreadable/erroring resource as a valid no-home state.
    const recovery = await page.evaluate(async selected => {
      const agent = window.store.getAgent();
      const derived = await agent?.privateDriveSubject();
      const resource = await window.store.getResource(selected);

      return {
        derived,
        subject: resource.subject,
        ready: resource.isReady(),
        classes: resource.getClasses(),
        error: resource.error?.message ?? null,
      };
    }, drive);
    expect(drive).toBe(recovery.derived);
    expect(recovery.subject).toBe(drive);
    expect(recovery.ready).toBe(false);
    expect(recovery.classes).toEqual([]);
    expect(recovery.error).toMatch(/Resource not found|not found locally/i);
  }

  await expect
    .poll(() =>
      page.evaluate(
        () => JSON.parse(localStorage.getItem('drive') ?? 'null') || null,
      ),
    )
    .toBe(drive);

  return drive;
}

async function assertReadonlyHome(page: Page, expectedDrive: string | null) {
  await expectIdentityUnlocked(page, 'readonly');

  if (expectedDrive !== manifest!.resources.drive) {
    await expect(
      page.getByRole('heading', {
        name: 'Your data is on another device',
        exact: true,
      }),
    ).toBeVisible();
    await expect(
      page.getByRole('button', { name: 'Connect', exact: true }),
    ).toBeVisible();
    await expect(
      page.getByRole('button', { name: 'Skip for now', exact: true }),
    ).toBeVisible();
    const selected = await readonlySessionDrive(page);
    expect(selected).not.toBe(manifest!.resources.drive);

    if (expectedDrive !== null) expect(selected).toBe(expectedDrive);

    return selected;
  }

  expect(expectedDrive).toBe(manifest!.resources.drive);
  await expect
    .poll(() => page.evaluate(() => window.store.getDrive() || null))
    .toBe(expectedDrive);
  await expect(
    page.getByRole('main').getByRole('heading', {
      level: 1,
      name: 'Local navigation fixture',
      exact: true,
    }),
  ).toBeVisible();
  const home = await page.evaluate(async subject => {
    const resource = await window.store.getResource(subject);
    const [writable] = await resource.canWrite(
      window.store.getAgent()?.subject,
    );

    return {
      subject: resource.subject,
      name: resource.get('https://atomicdata.dev/properties/name'),
      classes: resource.getClasses(),
      error: resource.error?.message ?? null,
      writable,
    };
  }, expectedDrive);
  expect(home.subject).toBe(expectedDrive);
  expect(home.name).toBe('Local navigation fixture');
  expect(home.classes).toContain('https://atomicdata.dev/classes/Drive');
  expect(home.error).toBeNull();
  expect(home.writable).toBe(false);
  expect(await readonlySessionDrive(page)).toBe(expectedDrive);
  await expect(page.getByRole('button', { name: /edit/i })).toHaveCount(0);
  await expect(
    page.getByRole('button', { name: /Upload file\(s\)\.\.\./i }),
  ).toHaveCount(0);
  await expect(page.locator('input[type="file"]')).toHaveCount(0);

  return expectedDrive;
}

function watchDiagnostics(page: Page, expectedErrors: RegExp[] = []) {
  const entries: ScenarioDiagnostic[] = [];
  const seenErrors = new Set<string>();
  let document = 0;

  const isExpected = (message: string) => {
    for (const pattern of expectedErrors) {
      pattern.lastIndex = 0;

      if (pattern.test(message) && !seenErrors.has(pattern.source)) {
        seenErrors.add(pattern.source);

        return true;
      }
    }

    return false;
  };

  page.on('console', message => {
    const text = redact(message.text());
    entries.push({
      kind: `console.${message.type()}`,
      message: text,
      url: redact(message.location().url),
      expected: message.type() === 'error' && isExpected(text),
      document,
    });
  });

  page.on('pageerror', error => {
    const message = redact(error.message);
    entries.push({
      kind: 'pageerror',
      message,
      url: redact(page.url()),
      expected: isExpected(message),
    });
  });

  page.on('request', request => {
    // SPA route changes do not restart the worker. Count only real main-frame
    // document requests so initialization credits survive in-app navigation.
    if (request.isNavigationRequest() && request.frame() === page.mainFrame()) {
      document += 1;
    }

    entries.push({
      kind: 'request',
      message: request.method(),
      url: redact(request.url()),
    });
  });

  page.on('requestfailed', request => {
    entries.push({
      kind: 'requestfailed',
      message: redact(request.failure()?.errorText ?? 'Request failed'),
      url: redact(request.url()),
    });
  });

  page.on('response', response => {
    entries.push({
      kind: 'http',
      message: redact(response.statusText()),
      status: response.status(),
      url: redact(response.url()),
    });
  });

  let socketId = 0;

  page.on('websocket', socket => {
    const id = ++socketId;
    let principal: string | null = null;
    let authenticatingPrincipal: string | null = null;
    const reads: ScenarioDiagnostic[] = [];
    // Keep one credit per actual outbound read, not an arbitrary warning quota.
    // Auth payloads are never attached; only the acknowledged public agent is.
    entries.push({
      kind: 'websocket.open',
      message: 'Opened',
      url: redact(socket.url()),
      socket: id,
    });

    // Binary tags and big-endian fields mirror lib/src/ws-v2.ts. Decode only
    // public routing/error metadata, never record AUTH proofs or commit bodies.
    socket.on('framesent', frame => {
      const payload = frame.payload;
      const entry: ScenarioDiagnostic = {
        kind: 'websocket.sent',
        message:
          typeof payload === 'string'
            ? `Text (${payload.length} characters)`
            : `Tag ${payload[0]} (${payload.byteLength} bytes)`,
        url: redact(socket.url()),
        socket: id,
        principal,
      };

      if (typeof payload !== 'string') {
        if (payload[0] === 0x01) {
          try {
            const auth = JSON.parse(payload.toString('utf8', 1));
            const agent = auth['https://atomicdata.dev/properties/auth/agent'];
            authenticatingPrincipal = typeof agent === 'string' ? agent : null;
          } catch {
            authenticatingPrincipal = null;
          }
        } else if (payload[0] === 0x20) {
          entry.operation = 'SUB';
          entry.resource = payload.toString('utf8', 1);
        } else if (payload[0] === 0x30 && payload.length >= 3) {
          const length = payload.readUInt16BE(1);

          if (payload.length >= 3 + length) {
            entry.operation = 'SYNC';
            entry.resource = payload.toString('utf8', 3, 3 + length);
          }
        }
      }

      if (entry.operation) reads.push(entry);

      entries.push(entry);
    });

    socket.on('framereceived', frame => {
      const payload = frame.payload;
      const entry: ScenarioDiagnostic = {
        kind: 'websocket.received',
        message:
          typeof payload === 'string'
            ? `Text (${payload.length} characters)`
            : `Tag ${payload[0]} (${payload.byteLength} bytes)`,
        url: redact(socket.url()),
        socket: id,
        principal,
      };

      if (typeof payload !== 'string') {
        if (payload[0] === 0x02) {
          principal = authenticatingPrincipal ?? '[unresolved-authentication]';
          entry.principal = principal;
        } else if (payload[0] === 0x03 && payload.length >= 5) {
          const error = {
            requestId: payload.readUInt16BE(1),
            code: payload.readUInt16BE(3),
            message: payload.toString('utf8', 5),
          };
          entry.message = redact(error.message);
          entry.status = error.code;
          // engine.rs gates SUB/SYNC against this session's read ACL; the
          // SDK logs code 7 without failing unrelated public-resource reads.
          const denied =
            /^(SUB|SYNC) refused for (\S+): Unauthorized\. This resource is not publicly readable\. Try signing in$/.exec(
              error.message,
            );

          if (denied) {
            entry.operation = denied[1] as 'SUB' | 'SYNC';
            entry.resource = denied[2];

            const requestIndex = reads.findIndex(
              read =>
                read.operation === entry.operation &&
                read.resource === entry.resource &&
                read.principal === null,
            );
            entry.expected =
              error.requestId === 0 &&
              error.code === 7 &&
              principal === null &&
              entry.resource.replace(/^did:ad:/, 'atomic:') ===
                manifest!.resources.drive.replace(/^did:ad:/, 'atomic:') &&
              requestIndex >= 0;

            if (entry.expected) reads.splice(requestIndex, 1);
          }
        }
      }

      entries.push(entry);
    });

    socket.on('close', () => {
      entries.push({
        kind: 'websocket.close',
        message: 'Closed',
        url: redact(socket.url()),
        socket: id,
        principal,
      });
    });

    socket.on('socketerror', error => {
      entries.push({
        kind: 'websocket.error',
        message: redact(error),
        url: redact(socket.url()),
        socket: id,
        principal,
      });
    });
  });

  return entries;
}

async function recordScenario(
  page: Page,
  testInfo: TestInfo,
  scenario: {
    route: string;
    user: string;
    viewport: Viewport;
    expected: string;
    expectedOpfsFailure?: boolean;
  },
  diagnostics: ScenarioDiagnostic[],
) {
  const protectedReads = () =>
    diagnostics.filter(
      entry =>
        entry.kind === 'websocket.received' && entry.operation !== undefined,
    );

  if (protectedReads().length > 0 || scenario.expectedOpfsFailure) {
    await page.waitForLoadState('networkidle', { timeout: 10_000 });
  }

  const settledReadDenials = protectedReads().length;
  const workerStarts = () =>
    diagnostics.filter(
      entry =>
        entry.kind === 'console.debug' &&
        entry.message === '[OwnerNavOPFS] worker started',
    );
  const settledWorkerStarts = workerStarts().length;

  if (settledReadDenials > 0 || scenario.expectedOpfsFailure) {
    // Matching a real denied request explains a warning, not an endless retry
    // loop. Observe quiescence after the exact route/history screen has settled.
    await page.waitForTimeout(1000);
  }

  const warningCredits = diagnostics.filter(
    entry => entry.kind === 'websocket.received' && entry.expected === true,
  );

  for (const entry of diagnostics) {
    if (entry.kind !== 'console.warning') continue;
    const index = warningCredits.findIndex(
      denial => entry.message === `[WS] refused: ${denial.message}`,
    );

    if (index >= 0) {
      entry.expected = true;
      entry.operation = warningCredits[index].operation;
      entry.resource = warningCredits[index].resource;
      entry.principal = warningCredits[index].principal;
      entry.socket = warningCredits[index].socket;
      warningCredits.splice(index, 1);
    }
  }

  if (scenario.expectedOpfsFailure) {
    const storage = await page.evaluate(() => window.store.getSyncStatus());
    expect(storage.clientDbReady).toBe(false);
    expect(storage.clientDbError).toMatch(
      /^Local caching and offline support are disabled:/,
    );
    expect(storage.clientDbError).toContain(
      'reading directly from the server; nothing is kept locally between reloads.',
    );
    const notice = storage.clientDbError!;
    const normalizedError = redact(`StorageBlockedError: ${notice}`);
    const storageCredits = new Map<string, number>();

    for (const entry of diagnostics) {
      let category: string | undefined;

      if (
        entry.kind === 'console.warning' &&
        entry.message.startsWith(
          '[ClientDb] legacy DB migration failed: Error: check legacy: JsValue(SecurityError: Storage access denied by test\n',
        ) &&
        /\/client-db\.worker[^/]*\.js(?:$|\?)/.test(entry.url ?? '')
      ) {
        category = 'legacy-migration';
      } else if (
        entry.kind === 'console.warning' &&
        entry.message === `[ClientDb] ${notice}`
      ) {
        category = 'storage-unavailable-notice';
      } else if (
        entry.kind === 'console.error' &&
        entry.message.startsWith(`${normalizedError} `)
      ) {
        category = 'storage-unavailable-notification';
      }

      if (!category) continue;
      const deniedInDocument = diagnostics.some(
        proof =>
          proof.document === entry.document &&
          proof.kind === 'console.debug' &&
          proof.message === '[OwnerNavOPFS] getDirectory denied',
      );
      const initializations = workerStarts().filter(
        start => start.document === entry.document,
      ).length;
      const key = `${entry.document}:${category}`;
      const used = storageCredits.get(key) ?? 0;

      // One source-established diagnostic per actual denied worker startup,
      // not a blanket SecurityError allowance or arbitrary repetition count.
      if (deniedInDocument && used < initializations) {
        entry.expected = true;
        storageCredits.set(key, used + 1);
      }
    }
  }

  const observed = {
    url: redact(page.url()),
    heading: (await page.getByRole('heading').allTextContents()).map(text =>
      redact(text.trim()),
    ),
    visibleText: redact(
      (await page.locator('body').innerText()).slice(0, 5000),
    ),
    diagnostics,
    settledReadDenials,
    finalReadDenials: protectedReads().length,
    settledWorkerStarts,
    finalWorkerStarts: workerStarts().length,
  };
  await testInfo.attach('scenario-observation.json', {
    body: JSON.stringify({ ...scenario, observed }, null, 2),
    contentType: 'application/json',
  });
  const screenshotPath = testInfo.outputPath(
    `scenario-${scenario.user}-${scenario.route}.png`,
  );
  await page.screenshot({
    path: screenshotPath,
    fullPage: true,
    mask: [
      page.locator('input'),
      page.locator('textarea'),
      page.locator('[contenteditable="true"]'),
    ],
  });
  await testInfo.attach(`scenario-${scenario.user}-${scenario.route}.png`, {
    path: screenshotPath,
    contentType: 'image/png',
  });
  const unexpected = diagnostics.filter(
    entry =>
      ['console.warning', 'console.error', 'pageerror'].includes(entry.kind) &&
      !entry.expected,
  );
  expect(
    unexpected,
    'Unexpected console diagnostics are attached in scenario-observation.json',
  ).toEqual([]);
  expect(
    protectedReads().filter(entry => !entry.expected),
    'Only the seeded private-drive read ACL denial for an anonymous wire principal is expected',
  ).toEqual([]);
  expect(
    protectedReads().length,
    'Protected reads must not retry while the final screen is settled',
  ).toBe(settledReadDenials);
  expect(
    workerStarts().length,
    'A settled server-only screen must not keep restarting its denied worker',
  ).toBe(settledWorkerStarts);
  await expect(
    page.getByRole('heading', { name: /^Unauthorized(?:\.|$)/ }),
  ).toHaveCount(0);
  await expect(page.getByText(/^Unauthorized[.:]/)).toHaveCount(0);
}

async function assertRouteContent(
  page: Page,
  route: (typeof routes)[number]['key'],
  role: Role | 'anonymous',
  expectedReadonlyDrive: string | null,
) {
  if (
    role === 'anonymous' &&
    ['app', 'newResource', 'privateResource', 'invite', 'table'].includes(route)
  ) {
    const destination = routeUrl(route);
    const expectedNext = `${new URL(destination).pathname}${new URL(destination).search}`;
    await expect
      .poll(() => {
        const parsed = new URL(page.url());

        return (
          parsed.pathname.includes('welcome') &&
          parsed.searchParams.get('next') === expectedNext
        );
      })
      .toBe(true);
    await openSignIn(page);

    return;
  }

  if (route === 'root' || route === 'app') {
    if (role === 'anonymous') {
      await openSignIn(page);
    } else if (role === 'readonly') {
      await assertReadonlyHome(page, expectedReadonlyDrive);
    } else {
      await expect(
        page.getByRole('main').getByRole('heading', {
          level: 1,
          name: 'Local navigation fixture',
          exact: true,
        }),
      ).toBeVisible();
    }

    return;
  }

  if (route === 'publicResource') {
    await expect(
      page.getByText('Public fixture content readable without signing in.', {
        exact: true,
      }),
    ).toBeVisible();

    return;
  }

  if (route === 'privateResource') {
    await expect(
      page.getByText('Private fixture content visible to drive members.', {
        exact: true,
      }),
    ).toBeVisible();

    return;
  }

  if (route === 'settings') {
    await expect(
      page.getByRole('heading', { name: 'Settings', exact: true }),
    ).toBeVisible();
    await expect(
      page.getByRole('textbox', { name: 'Search settings' }),
    ).toBeVisible();

    return;
  }

  if (route === 'newResource') {
    if (role === 'readonly') {
      await expect(page.getByText(/read.only/i).first()).toBeVisible();
      await expect(
        page.getByRole('button', { name: 'Document', exact: true }),
      ).toHaveCount(0);
      await expect(
        page.getByRole('button', { name: /Upload file\(s\)\.\.\./i }),
      ).toHaveCount(0);
    } else {
      await expect(
        page.getByRole('heading', {
          name: 'Create something new',
          exact: true,
        }),
      ).toBeVisible();
      await expect(
        page.getByRole('searchbox', {
          name: 'Search templates and resource types',
        }),
      ).toBeVisible();
    }

    return;
  }

  if (route === 'invite') {
    await expect(
      page.getByRole('main').getByRole('heading', {
        level: 1,
        name: 'Local navigation fixture',
        exact: true,
      }),
    ).toBeVisible();

    return;
  }

  if (route === 'table') {
    await expect(
      page.getByRole('heading', {
        name: 'Navigation fixture table',
        exact: true,
      }),
    ).toBeVisible();
    const grid = page.getByRole('grid');
    await expect(grid).toBeVisible();
    await expect.poll(() => grid.getByRole('row').count()).toBeGreaterThan(1);

    return;
  }
}

async function assertRouteScreen(
  page: Page,
  route: (typeof routes)[number]['key'],
  role: Role | 'anonymous',
  expectedReadonlyDrive: string | null = null,
) {
  await assertRouteContent(page, route, role, expectedReadonlyDrive);
  await expect(
    page.getByRole('heading', { name: /^Unauthorized(?:\.|$)/ }),
  ).toHaveCount(0);
  await expect(page.getByText(/^Unauthorized[.:]/)).toHaveCount(0);
}

if (manifest && identities) {
  test.describe('seeded owner-mode browser navigation', () => {
    test.beforeEach(async ({ page }) => {
      await installExternalFixtures(page.context());
    });
    test.afterEach(async ({ page }) => {
      await page
        .evaluate(() => {
          const url = new URL(location.href);
          const secretParameters = ['token', 'secret', 'inviteToken'];
          for (const key of secretParameters) url.searchParams.delete(key);
          const next = url.searchParams.get('next');

          if (next) {
            const nested = new URL(next, location.href);
            for (const key of secretParameters) nested.searchParams.delete(key);
            url.searchParams.set(
              'next',
              `${nested.pathname}${nested.search}${nested.hash}`,
            );
          }

          history.replaceState(
            history.state,
            '',
            `${url.pathname}${url.search}${url.hash}`,
          );
        })
        .catch(() => undefined);
      await Promise.all(
        [...isolatedContexts].map(context =>
          context.close().catch(() => undefined),
        ),
      );
      isolatedContexts.clear();
    });

    for (const route of routes) {
      for (const role of ['anonymous', 'owner', 'readonly'] as const) {
        for (const viewport of Object.keys(viewports) as Viewport[]) {
          test(`${route.name}: ${role} at ${viewport}`, async ({
            page,
          }, testInfo) => {
            await page.setViewportSize(viewports[viewport]);

            if (viewport === 'mobile') {
              await page.addInitScript(() =>
                localStorage.setItem('sideBarOpen', JSON.stringify(true)),
              );
            }

            const diagnostics = watchDiagnostics(page);
            const commitAttempts =
              role === 'anonymous' || role === 'readonly'
                ? observeCommitAttempts(page)
                : undefined;
            const destination = routeUrl(route.key);
            await visitAs(page, destination, role);
            await assertRouteScreen(page, route.key, role);
            await safeNavigation(() => page.reload());
            await assertRouteScreen(page, route.key, role);
            const readonlyDrive =
              role === 'readonly' ? await readonlySessionDrive(page) : null;
            const historyAnchor = route.key === 'root' ? 'settings' : 'root';
            await safeNavigation(() => page.goto(routeUrl(historyAnchor)));
            await assertRouteScreen(page, historyAnchor, role, readonlyDrive);
            await safeNavigation(() => page.goBack());
            await assertRouteScreen(page, route.key, role, readonlyDrive);
            await safeNavigation(() => page.goForward());
            await assertRouteScreen(page, historyAnchor, role, readonlyDrive);
            await safeNavigation(() => page.goBack());
            await assertRouteScreen(page, route.key, role, readonlyDrive);
            await expect(
              page.getByText(/Resource not found|not found locally/i),
            ).toHaveCount(0);

            if (
              ['publicResource', 'privateResource'].includes(route.key) &&
              role !== 'owner'
            ) {
              await expect(
                page.getByRole('button', { name: /Upload file\(s\)\.\.\./i }),
              ).toHaveCount(0);
              await expect(page.locator('input[type="file"]')).toHaveCount(0);
            }

            if (role === 'anonymous') {
              await expect(
                page.locator(
                  '[data-testid="account-menu-trigger"][data-signed-in="true"]',
                ),
              ).toHaveCount(0);
              const localSession = await page.evaluate(() => ({
                agent: window.store.getAgent()?.subject ?? null,
                drive: window.store.getDrive() ?? null,
              }));
              expect(localSession.agent).toBeNull();

              if (route.key === 'publicResource') {
                const subject = new URL(destination).searchParams.get(
                  'subject',
                );
                expect(subject).not.toBeNull();
                const publicDrive = await page.evaluate(async openedSubject => {
                  const resource =
                    await window.store.getResource(openedSubject);

                  return {
                    drive: resource.get(
                      'https://atomicdata.dev/properties/drive',
                    ),
                    error: resource.error?.message ?? null,
                  };
                }, subject!);
                expect(publicDrive.error).toBeNull();
                expect(publicDrive.drive).toBe(manifest.resources.drive);
                expect(localSession.drive).toBe(publicDrive.drive);
              } else {
                expect(localSession.drive).toBeNull();
              }

              expectNoCommitAttempts(commitAttempts!);
            }

            if (role === 'readonly') {
              expect(await readonlySessionDrive(page)).toBe(readonlyDrive);
              expectNoCommitAttempts(commitAttempts!);
            }

            await recordScenario(
              page,
              testInfo,
              {
                route: route.name,
                user: role,
                viewport,
                expected:
                  role === 'anonymous'
                    ? 'public content or usable sign-in with preserved destination'
                    : `${role} can read seeded route with role-appropriate actions`,
              },
              diagnostics,
            );
          });
        }
      }
    }

    for (const route of routes) {
      test(`${route.name}: anonymous navigation survives reload and history`, async ({
        page,
      }, testInfo) => {
        await page.setViewportSize(viewports.mobile);
        await page.addInitScript(() =>
          localStorage.setItem('sideBarOpen', JSON.stringify(true)),
        );
        const diagnostics = watchDiagnostics(page);
        const target = routeUrl(route.key);
        await safeNavigation(() => page.goto(routeUrl('root')));
        await safeNavigation(() => page.goto(target));
        await assertRouteScreen(page, route.key, 'anonymous');
        const finalUrl = page.url();
        await safeNavigation(() => page.reload());
        await assertRouteScreen(page, route.key, 'anonymous');
        await safeNavigation(() => page.goBack());
        await assertRouteScreen(page, 'root', 'anonymous');
        await safeNavigation(() => page.goForward());
        await assertRouteScreen(page, route.key, 'anonymous');
        await expect.poll(() => page.url() === finalUrl).toBe(true);
        await expect(
          page.getByText(/Resource not found|not found locally|did:ad:[^\s]+/i),
        ).toHaveCount(0);

        if (
          ['app', 'newResource', 'privateResource', 'invite', 'table'].includes(
            route.key,
          )
        ) {
          await openSignIn(page);
        } else {
          await assertRouteScreen(page, route.key, 'anonymous');
        }

        await recordScenario(
          page,
          testInfo,
          {
            route: route.name,
            user: 'anonymous',
            viewport: 'mobile',
            expected:
              'reload/back/forward remains a usable public view or sign-in screen',
          },
          diagnostics,
        );
      });
    }

    test('owner edit controls appear and read-only member controls do not on seeded resource', async ({
      browser,
    }, testInfo) => {
      const ownerContext = await browser.newContext({
        viewport: viewports.desktop,
      });
      isolatedContexts.add(ownerContext);
      await installExternalFixtures(ownerContext);
      const ownerPage = await ownerContext.newPage();
      const ownerDiagnostics = watchDiagnostics(ownerPage);
      await signIn(ownerPage, 'owner');
      await ownerPage.goto(routeUrl('privateResource'));
      // The seeded private resource is an Article; its real write affordance
      // is ArticleDescription's Edit content button, not a document editor.
      await assertRouteScreen(ownerPage, 'privateResource', 'owner');
      const editContent = ownerPage.getByRole('button', {
        name: 'Edit content',
        exact: true,
      });
      await expect(editContent).toBeVisible();
      await expect(editContent).toBeEnabled();
      await editContent.click({ trial: true });
      await expect(
        ownerPage.getByRole('button', { name: /Upload file\(s\)\.\.\./i }),
      ).toBeVisible();
      await expect(ownerPage.locator('input[type="file"]')).toHaveCount(1);
      const folder = await ownerPage.evaluate(async () => {
        const subject = new URL(location.href).searchParams.get('subject');

        if (!subject) throw new Error('Missing private fixture subject.');

        const article = await window.store.getResource(subject);
        const parent = article.get('https://atomicdata.dev/properties/parent');

        if (typeof parent !== 'string') {
          throw new Error(
            'The private fixture must have its seeded folder parent.',
          );
        }

        const resource = await window.store.getResource(parent);

        return { subject: resource.subject, classes: resource.getClasses() };
      });
      expect(folder.classes).toContain('https://atomicdata.dev/classes/Folder');
      const folderUrl = new URL(routeUrl('privateResource'));
      folderUrl.searchParams.set('subject', folder.subject);
      await safeNavigation(() => ownerPage.goto(folderUrl.toString()));
      await expect(
        ownerPage.getByRole('main').getByRole('heading', {
          level: 1,
          name: 'Private navigation folder',
          exact: true,
        }),
      ).toBeVisible();
      await ownerPage
        .getByRole('main')
        .getByRole('button', { name: /^(?:New|Create new resource)$/ })
        .click();
      await assertRouteScreen(ownerPage, 'newResource', 'owner');
      expect(new URL(ownerPage.url()).searchParams.get('parentSubject')).toBe(
        folder.subject,
      );
      const folderUpload = ownerPage.getByRole('button', {
        name: 'Drop files or click here to upload.',
        exact: true,
      });
      await expect(folderUpload).toBeVisible();
      await expect(folderUpload).toBeEnabled();
      await folderUpload.click({ trial: true });
      await expect(ownerPage.locator('input[type="file"]')).toHaveCount(1);
      await safeNavigation(() => ownerPage.goto(routeUrl('privateResource')));
      await assertRouteScreen(ownerPage, 'privateResource', 'owner');
      await recordScenario(
        ownerPage,
        testInfo,
        {
          route: 'privateResource',
          user: 'owner',
          viewport: 'desktop',
          expected:
            'owner reads the private Article with Edit content/upload controls and can open its folder-scoped upload picker',
        },
        ownerDiagnostics,
      );

      const readonlyContext = await browser.newContext({
        viewport: viewports.desktop,
      });
      isolatedContexts.add(readonlyContext);
      await installExternalFixtures(readonlyContext);
      const readonlyPage = await readonlyContext.newPage();
      const readonlyDiagnostics = watchDiagnostics(readonlyPage);
      await signIn(readonlyPage, 'readonly');
      await readonlyPage.goto(routeUrl('privateResource'));
      await expect(
        readonlyPage.getByText(
          'Private fixture content visible to drive members.',
          { exact: true },
        ),
      ).toBeVisible();
      await expect(
        readonlyPage.getByRole('button', { name: /edit/i }),
      ).toHaveCount(0);
      await expect(
        readonlyPage.getByRole('button', { name: /Upload file\(s\)\.\.\./i }),
      ).toHaveCount(0);
      await expect(readonlyPage.locator('input[type="file"]')).toHaveCount(0);
      await safeNavigation(() => readonlyPage.goto(folderUrl.toString()));
      await expect(
        readonlyPage.getByRole('main').getByRole('heading', {
          level: 1,
          name: 'Private navigation folder',
          exact: true,
        }),
      ).toBeVisible();
      await expect(
        readonlyPage.getByRole('main').getByRole('button', {
          name: /^(?:New|Create new resource)$/,
        }),
      ).toHaveCount(0);
      const folderCreation = new URL(routeUrl('newResource'));
      folderCreation.searchParams.set('parentSubject', folder.subject);
      await safeNavigation(() => readonlyPage.goto(folderCreation.toString()));
      await assertRouteScreen(readonlyPage, 'newResource', 'readonly');
      await expect(
        readonlyPage.getByRole('button', {
          name: 'Drop files or click here to upload.',
          exact: true,
        }),
      ).toHaveCount(0);
      await expect(readonlyPage.locator('input[type="file"]')).toHaveCount(0);
      await safeNavigation(() =>
        readonlyPage.goto(routeUrl('privateResource')),
      );
      await assertRouteScreen(readonlyPage, 'privateResource', 'readonly');
      await recordScenario(
        readonlyPage,
        testInfo,
        {
          route: 'privateResource',
          user: 'readonly',
          viewport: 'desktop',
          expected:
            'read-only member reads the same private Article without edit/upload controls and cannot create/upload in its folder',
        },
        readonlyDiagnostics,
      );
    });

    for (const host of [
      new URL(manifest.origin).hostname,
      `alternate.${new URL(manifest.origin).hostname}`,
    ]) {
      for (const role of ['owner', 'readonly'] as const) {
        test(`fresh-profile ${role} sign-in and shared-drive handover on ${host}`, async ({
          browser,
        }, testInfo) => {
          const context = await browser.newContext({
            viewport: viewports.desktop,
          });
          isolatedContexts.add(context);
          await installExternalFixtures(context);
          const page = await context.newPage();
          const diagnostics = watchDiagnostics(page);
          const commitAttempts = observeCommitAttempts(page);
          const refusedCommitUrls: string[] = [];
          page.on('response', response => {
            if (response.status() >= 400 && /commit/i.test(response.url()))
              refusedCommitUrls.push(response.url());
          });
          const home = routeUrl('privateResource', host);
          await safeNavigation(() => page.goto(home));
          await expectStoreReady(page);
          await expect(
            page.locator(
              '[data-testid="account-menu-trigger"][data-signed-in="true"]',
            ),
          ).toHaveCount(0);
          const localSession = await page.evaluate(() => ({
            agent: window.store.getAgent()?.subject ?? null,
            drive: window.store.getDrive() ?? null,
          }));
          expect(localSession.agent).toBeNull();
          expect(localSession.drive).toBeNull();
          const secret = identities.identities[role].secret;
          await openSignIn(page);
          await fillSecret(page, secret);
          await expectIdentityUnlocked(page, role);
          await expect(
            page.getByText(
              'Private fixture content visible to drive members.',
              { exact: true },
            ),
          ).toBeVisible({ timeout: 30_000 });
          await page.waitForLoadState('networkidle', { timeout: 30_000 });
          await expect
            .poll(
              () =>
                page.evaluate(() => ({
                  current: window.store.getAgent()?.subject ?? null,
                  drive: window.store.getDrive() ?? null,
                })),
              { timeout: 30_000 },
            )
            .toEqual({
              current: identities.identities[role].subject,
              drive: manifest.resources.drive,
            });
          expectNoCommitAttempts(commitAttempts);
          expect(refusedCommitUrls).toEqual([]);
          await recordScenario(
            page,
            testInfo,
            {
              route: 'privateResource',
              user: role,
              viewport: 'desktop',
              expected: `existing ${role} identity signs in on ${host} and remains in shared seeded drive`,
            },
            diagnostics,
          );
        });
      }
    }

    for (const host of [
      new URL(manifest.origin).hostname,
      `alternate.${new URL(manifest.origin).hostname}`,
    ]) {
      test(`read-only existing identity at root does not create a personal drive on ${host}`, async ({
        browser,
      }, testInfo) => {
        const context = await browser.newContext({
          viewport: viewports.desktop,
        });
        isolatedContexts.add(context);
        await installExternalFixtures(context);
        const page = await context.newPage();
        const diagnostics = watchDiagnostics(page);
        const commitAttempts = observeCommitAttempts(page);
        const refusedCommitUrls: string[] = [];
        page.on('response', response => {
          if (response.status() >= 400 && /commit/i.test(response.url()))
            refusedCommitUrls.push(response.url());
        });
        await safeNavigation(() => page.goto(routeUrl('root', host)));
        await expectStoreReady(page);
        expect(
          await page.evaluate(() => window.store.getAgent()?.subject ?? null),
        ).toBeNull();
        expect(
          await page.evaluate(() => window.store.getDrive() ?? null),
        ).toBeNull();
        await openSignIn(page);
        await fillSecret(page, identities.identities.readonly.secret);
        await expectIdentityUnlocked(page, 'readonly');
        const recoveryDrive = await assertReadonlyHome(page, null);
        await page.waitForLoadState('networkidle', { timeout: 30_000 });
        await expect
          .poll(() => page.evaluate(() => window.store.getDrive() ?? null), {
            timeout: 30_000,
          })
          .toBe(recoveryDrive);
        expectNoCommitAttempts(commitAttempts);
        expect(refusedCommitUrls).toEqual([]);
        await recordScenario(
          page,
          testInfo,
          {
            route: 'root',
            user: 'readonly',
            viewport: 'desktop',
            expected:
              'existing read-only identity reaches the no-data recovery screen without personal-drive genesis',
          },
          diagnostics,
        );
      });
    }

    test('actual worker OPFS SecurityError reports server-only reads and reloads', async ({
      page,
    }, testInfo) => {
      let workerIntercepted = false;
      await page.addInitScript(() => {
        const channel = new BroadcastChannel('atomic-owner-nav-opfs-denied');

        channel.onmessage = () => {
          (
            window as Window & { ownerNavOpfsDenied?: boolean }
          ).ownerNavOpfsDenied = true;
        };
      });
      await page.route('**/client-db.worker*.js*', async route => {
        const response = await route.fetch();
        const source = await response.text();
        workerIntercepted = true;
        await route.fulfill({
          response,
          body: `console.debug('[OwnerNavOPFS] worker started'); Object.defineProperty(navigator.storage, 'getDirectory', { configurable: true, value: async () => { console.debug('[OwnerNavOPFS] getDirectory denied'); new BroadcastChannel('atomic-owner-nav-opfs-denied').postMessage('denied'); throw new DOMException('Storage access denied by test', 'SecurityError'); } });\n${source}`,
        });
      });
      const diagnostics = watchDiagnostics(page);
      await safeNavigation(() => page.goto(routeUrl('root')));
      await expect
        .poll(() =>
          page.evaluate(() =>
            Boolean(
              (window as Window & { ownerNavOpfsDenied?: boolean })
                .ownerNavOpfsDenied,
            ),
          ),
        )
        .toBe(true);
      expect(workerIntercepted).toBe(true);
      await signIn(page, 'owner');
      await expect(
        page.getByText(/Local caching and offline support are disabled/i),
      ).toBeVisible();
      await expect(
        page.getByText(/reading directly from the server/i),
      ).toBeVisible();
      await expect(
        page.getByText(/nothing is kept locally between reloads/i),
      ).toBeVisible();
      await safeNavigation(() => page.goto(routeUrl('publicResource')));
      await expect(
        page.getByText('Public fixture content readable without signing in.', {
          exact: true,
        }),
      ).toBeVisible();
      await safeNavigation(() => page.reload());
      await expect(
        page.getByText('Public fixture content readable without signing in.', {
          exact: true,
        }),
      ).toBeVisible();
      await recordScenario(
        page,
        testInfo,
        {
          route: 'publicResource',
          user: 'owner',
          viewport: 'desktop',
          expected:
            'real worker OPFS SecurityError yields an honest server-backed read mode that reloads',
          expectedOpfsFailure: true,
        },
        diagnostics,
      );
    });

    test('outsider drive genesis gets one 403, retains edits, and offers existing-identity sign-in', async ({
      browser,
      page,
    }, testInfo) => {
      const diagnostics = watchDiagnostics(page, [
        /does not host new drives|owner.mode.*(?:refus|reject)/i,
      ]);
      let webSocketsSeen = 0;
      let webSocketsClosed = 0;
      page.on('websocket', socket => {
        webSocketsSeen += 1;
        socket.on('close', () => {
          webSocketsClosed += 1;
        });
      });
      const commitAttempts = observeCommitAttempts(page);

      const httpContext = await browser.newContext({
        viewport: viewports.desktop,
      });
      isolatedContexts.add(httpContext);
      await installExternalFixtures(httpContext);
      const httpPage = await httpContext.newPage();
      const httpAttempts = observeCommitAttempts(httpPage);
      const httpRefusedResponses: Array<{ url: string; status: number }> = [];
      httpPage.on('response', response => {
        if (response.status() === 403 && /commit/i.test(response.url())) {
          httpRefusedResponses.push({
            url: response.url(),
            status: response.status(),
          });
        }
      });
      await safeNavigation(() => httpPage.goto(routeUrl('root')));
      await openSignIn(httpPage);
      await fillSecret(httpPage, identities.identities.outsider.secret);
      await expectIdentityUnlocked(httpPage, 'outsider');
      await expect
        .poll(() => httpPage.evaluate(() => window.store.serverConnected), {
          timeout: 20_000,
        })
        .toBe(true);
      const httpSave = await httpPage.evaluate(async () => {
        const endpoint = new URL(
          '/commit',
          window.store.getServerUrl(),
        ).toString();
        window.store.disconnect();
        const websocketAvailable = Boolean(
          window.store.getWebSocketForSubject(endpoint),
        );
        const resource = await window.store.newResource({
          isA: 'https://atomicdata.dev/classes/Drive',
          noParent: true,
          deferGenesis: true,
          propVals: {
            'https://atomicdata.dev/properties/name':
              'Browser HTTP owner-mode refusal probe',
          },
        });
        const commit = await resource.signChanges();

        try {
          await window.store.postCommit(commit, endpoint);

          return { websocketAvailable, error: null };
        } catch (error) {
          return {
            websocketAvailable,
            error: error instanceof Error ? error.message : String(error),
          };
        }
      });
      await expect
        .poll(() => httpRefusedResponses.length, { timeout: 20_000 })
        .toBeGreaterThan(0);
      expect(httpRefusedResponses).toHaveLength(1);
      expect(httpRefusedResponses[0].status).toBe(403);
      expect(httpRefusedResponses[0].url).toMatch(/commit/i);
      expect(httpAttempts.httpPosts).toHaveLength(1);
      expect(httpAttempts.websocketCommits).toBe(0);
      expect(httpSave.websocketAvailable).toBe(false);
      expect(httpSave.error).toMatch(/does not host new drives|owner.?mode/i);
      await testInfo.attach('http-owner-mode-refusal.json', {
        body: JSON.stringify(
          {
            httpCommitPosts: httpAttempts.httpPosts.map(redact),
            refusedResponses: httpRefusedResponses.map(response => ({
              url: redact(response.url),
              status: response.status,
            })),
            websocketAvailable: httpSave.websocketAvailable,
            websocketCommitFrames: httpAttempts.websocketCommits,
            refusal: redact(httpSave.error ?? ''),
          },
          null,
          2,
        ),
        contentType: 'application/json',
      });
      await httpContext.close();
      isolatedContexts.delete(httpContext);

      await signIn(page, 'outsider');
      await expect
        .poll(() => webSocketsSeen, { timeout: 20_000 })
        .toBeGreaterThan(0);
      await expect
        .poll(() => page.evaluate(() => window.store.serverConnected), {
          timeout: 20_000,
        })
        .toBe(true);
      expectNoCommitAttempts(commitAttempts);
      const commitFramesBeforeRefusal = commitAttempts.websocketCommits;
      const errorFramesBeforeRefusal = commitAttempts.websocketErrors;
      const attempt = await page.evaluate(async () => {
        const resource = await window.store.newResource({
          isA: 'https://atomicdata.dev/classes/Drive',
          noParent: true,
          propVals: {
            'https://atomicdata.dev/properties/name':
              'Browser owner-mode refusal attempt',
          },
        });
        await resource.set(
          'https://atomicdata.dev/properties/name',
          'Retained edit after owner-mode refusal',
        );

        try {
          await resource.save();

          return { subject: resource.subject, error: null };
        } catch (error) {
          return {
            subject: resource.subject,
            error: error instanceof Error ? error.message : String(error),
          };
        }
      });
      await expect
        .poll(() => commitAttempts.websocketCommits, { timeout: 20_000 })
        .toBeGreaterThan(commitFramesBeforeRefusal);
      expect(commitAttempts.websocketCommits).toBe(
        commitFramesBeforeRefusal + 1,
      );
      await expect
        .poll(() => commitAttempts.websocketErrors, { timeout: 20_000 })
        .toBeGreaterThan(errorFramesBeforeRefusal);
      expect(commitAttempts.httpPosts).toHaveLength(0);
      expect(attempt.error).toMatch(/does not host new drives|owner.?mode/i);
      const postsAfterRefusal = commitAttempts.httpPosts.length;
      const framesAfterRefusal = commitAttempts.websocketCommits;
      const retainedName = await page.evaluate(async subject => {
        const resource = await window.store.getResource(subject);

        return resource.get('https://atomicdata.dev/properties/name');
      }, attempt.subject);
      expect(retainedName).toBe('Retained edit after owner-mode refusal');
      await expect(
        page
          .getByText(
            /owner.mode|does not host new drives|cannot create.*drive/i,
          )
          .first(),
      ).toBeVisible();
      const refusalGuidance = page.getByText(
        /Sign in with an existing identity to use a workspace hosted here\./,
      );
      await expect(refusalGuidance).toBeVisible();
      const guidanceText = redact(await refusalGuidance.innerText());
      const socketsBeforeReconnect = webSocketsSeen;
      const closesBeforeOffline = webSocketsClosed;
      await page.context().setOffline(true);
      await expect
        .poll(() => webSocketsClosed, { timeout: 20_000 })
        .toBeGreaterThan(closesBeforeOffline);
      await page.context().setOffline(false);
      await expect
        .poll(() => webSocketsSeen, { timeout: 30_000 })
        .toBeGreaterThan(socketsBeforeReconnect);
      await expect
        .poll(() => page.evaluate(() => window.store.serverConnected), {
          timeout: 30_000,
        })
        .toBe(true);
      await page.waitForTimeout(35_000);
      expect(commitAttempts.httpPosts).toHaveLength(postsAfterRefusal);
      expect(commitAttempts.websocketCommits).toBe(framesAfterRefusal);
      const retainedAfterReconnect = await page.evaluate(async subject => {
        const resource = await window.store.getResource(subject);

        return resource.get('https://atomicdata.dev/properties/name');
      }, attempt.subject);
      expect(retainedAfterReconnect).toBe(
        'Retained edit after owner-mode refusal',
      );
      await testInfo.attach('websocket-owner-mode-refusal.json', {
        body: JSON.stringify(
          {
            websocketCommitFrames: framesAfterRefusal,
            websocketErrorFrames: commitAttempts.websocketErrors,
            httpCommitPostsAfterReconnect: commitAttempts.httpPosts.length,
            retainedEdit: retainedName,
            refusal: redact(attempt.error ?? ''),
            guidance: guidanceText,
            retainedEditAfterReconnect: retainedAfterReconnect,
          },
          null,
          2,
        ),
        contentType: 'application/json',
      });
      // Toaster renders SDK guidance with Clear/Copy, not a sign-in button.
      // Prove its supported handover destination is a real usable sign-in page
      // only AFTER proving live retained-edit/reconnect/no-retry behavior.
      const destination = new URL(routeUrl('privateResource'));
      const next = `${destination.pathname}${destination.search}`;
      const handover = new URL('/app/welcome', manifest.origin);
      handover.searchParams.set('step', 'signin');
      handover.searchParams.set('next', next);
      await safeNavigation(() => page.goto(handover.toString()));
      await openSignIn(page);
      expect(new URL(page.url()).searchParams.get('next')).toBe(next);
      expect(commitAttempts.httpPosts).toHaveLength(postsAfterRefusal);
      expect(commitAttempts.websocketCommits).toBe(framesAfterRefusal);
      await recordScenario(
        page,
        testInfo,
        {
          route: 'newResource',
          user: 'outsider',
          viewport: 'desktop',
          expected:
            'actual HTTP 403 and WebSocket refusal; no later HTTP commit or WS send; retained edit and existing-identity handover',
        },
        diagnostics,
      );
    });
  });
}
