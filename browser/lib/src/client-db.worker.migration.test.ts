import { afterEach, beforeEach, expect, it, vi } from 'vitest';

const db = vi.hoisted(() => ({
  indexMigrationPending: vi.fn(),
  migrateIndexKeysStep: vi.fn(),
  openClientDb: vi.fn(),
  messageMigrationPending: vi.fn(),
  migrateMessagesStep: vi.fn(),
  aiChatMigrationPending: vi.fn(),
  migrateAiChatsStep: vi.fn(),
}));
vi.mock('./client-db-open.js', async importOriginal => {
  const actual = await importOriginal();

  return {
    ...actual,
    openClientDb: (...args: unknown[]) => db.openClientDb(...args),
  };
});
beforeEach(() => {
  db.openClientDb.mockResolvedValue({ db });
  db.indexMigrationPending.mockReturnValue(false);
  db.messageMigrationPending.mockReturnValue(false);
  db.aiChatMigrationPending.mockReturnValue(false);
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
  vi.resetAllMocks();
});

it('rebuilds the indexes in slices before init is acknowledged, reporting each one', async () => {
  const steps = [
    { done: 100, total: 250, finished: false },
    { done: 200, total: 250, finished: false },
    { done: 250, total: 250, finished: true },
  ];
  db.indexMigrationPending.mockReturnValue(true);
  db.migrateIndexKeysStep.mockImplementation(() =>
    JSON.stringify(steps.shift()),
  );

  const posted: Array<Record<string, unknown>> = [];
  const worker = {
    onmessage: null as unknown as (event: unknown) => void,
    postMessage: vi.fn((value: Record<string, unknown>) => posted.push(value)),
  };
  vi.stubGlobal('self', worker);
  await import('./client-db.worker.js');

  worker.onmessage({
    data: {
      id: 1,
      type: 'init',
      wasmUrl: 'data:text/javascript,export default async function() {}',
    },
  });
  await vi.waitFor(() => expect(posted.some(m => m.id === 1)).toBe(true));

  const progress = posted
    .filter(m => m.type === 'migration-progress')
    .map(m => [m.done, m.total, m.finished]);
  expect(progress).toEqual([
    [0, 0, false],
    [100, 250, false],
    [200, 250, false],
    [250, 250, true],
  ]);
  expect(posted.at(-1)).toMatchObject({ id: 1, type: 'ok' });
  expect(db.migrateIndexKeysStep).toHaveBeenCalledWith(100);
});

it('does nothing for a database that is already current', async () => {
  const posted: Array<Record<string, unknown>> = [];
  const worker = {
    onmessage: null as unknown as (event: unknown) => void,
    postMessage: vi.fn((value: Record<string, unknown>) => posted.push(value)),
  };
  vi.stubGlobal('self', worker);
  await import('./client-db.worker.js');

  worker.onmessage({
    data: {
      id: 1,
      type: 'init',
      wasmUrl: 'data:text/javascript,export default async function() {}',
    },
  });
  await vi.waitFor(() => expect(posted.some(m => m.id === 1)).toBe(true));

  expect(posted.some(m => m.type === 'migration-progress')).toBe(false);
  expect(db.migrateIndexKeysStep).not.toHaveBeenCalled();
});

it.each([
  [
    'a direct native refusal',
    new DOMException('storage access denied by test', 'SecurityError'),
  ],
  [
    'the wrapped legacy WASM refusal',
    new Error(
      'check legacy: JsValue(SecurityError: Storage access denied by test\nundefined)',
    ),
  ],
])(
  'settles %s during legacy migration before opening the new database',
  async (_case, refusal) => {
    const posted: Array<Record<string, unknown>> = [];
    const worker = {
      onmessage: null as unknown as (event: unknown) => void,
      postMessage: vi.fn((value: Record<string, unknown>) =>
        posted.push(value),
      ),
    };
    vi.stubGlobal('self', worker);
    await import('./client-db.worker.js');

    const throwRefusal =
      refusal instanceof DOMException
        ? `throw new DOMException(${JSON.stringify(refusal.message)}, ` +
          `${JSON.stringify(refusal.name)});`
        : `throw new Error(${JSON.stringify(refusal.message)});`;
    const wasmUrl =
      'data:text/javascript,' +
      encodeURIComponent(
        `export default async function() {}; export async function migrateLegacyClientDb() { ${throwRefusal} }`,
      );
    worker.onmessage({
      data: {
        id: 1,
        type: 'init',
        wasmUrl,
        dbName: 'agent.db',
        migrateLegacy: true,
      },
    });
    await vi.waitFor(() =>
      expect(posted.some(message => message.id === 1)).toBe(true),
    );

    expect(db.openClientDb).not.toHaveBeenCalled();
    expect(posted.at(-1)).toMatchObject({
      id: 1,
      type: 'error',
      message: expect.stringMatching(/ATOMIC_DB_STORAGE_BLOCKED/),
    });
  },
);

it('continues opening the new database after an unrelated legacy migration failure', async () => {
  const posted: Array<Record<string, unknown>> = [];
  const worker = {
    onmessage: null as unknown as (event: unknown) => void,
    postMessage: vi.fn((value: Record<string, unknown>) => posted.push(value)),
  };
  vi.stubGlobal('self', worker);
  await import('./client-db.worker.js');

  const wasmUrl =
    'data:text/javascript,' +
    encodeURIComponent(
      `export default async function() {}; export async function migrateLegacyClientDb() { throw new Error('legacy data is unsupported'); }`,
    );
  worker.onmessage({
    data: {
      id: 1,
      type: 'init',
      wasmUrl,
      dbName: 'agent.db',
      migrateLegacy: true,
    },
  });
  await vi.waitFor(() =>
    expect(posted.some(message => message.id === 1)).toBe(true),
  );

  expect(db.openClientDb).toHaveBeenCalledOnce();
  expect(posted.at(-1)).toMatchObject({ id: 1, type: 'ok' });
});

it('moves chat messages into pages after the index rebuild, for the local-only drives only', async () => {
  db.messageMigrationPending.mockReturnValue(true);
  const steps = [
    { done: 500, total: 700, finished: false },
    { done: 700, total: 700, finished: true },
  ];
  db.migrateMessagesStep.mockImplementation(async () =>
    JSON.stringify(steps.shift()),
  );

  const posted: Array<Record<string, unknown>> = [];
  const worker = {
    onmessage: null as unknown as (event: unknown) => void,
    postMessage: vi.fn((value: Record<string, unknown>) => posted.push(value)),
  };
  vi.stubGlobal('self', worker);
  await import('./client-db.worker.js');

  worker.onmessage({
    data: {
      id: 1,
      type: 'init',
      wasmUrl: 'data:text/javascript,export default async function() {}',
      localOnlyDrives: ['did:ad:local'],
    },
  });
  await vi.waitFor(() => expect(posted.some(m => m.id === 1)).toBe(true));

  expect(
    posted
      .filter(m => m.type === 'migration-progress')
      .map(m => [m.phase, m.done, m.total, m.finished]),
  ).toEqual([
    ['messages', 0, 0, false],
    ['messages', 500, 700, false],
    ['messages', 700, 700, true],
  ]);
  expect(db.migrateMessagesStep).toHaveBeenCalledWith(
    500,
    JSON.stringify(['did:ad:local']),
  );
  expect(posted.at(-1)).toMatchObject({ id: 1, type: 'ok' });
});

it('writes no pages when no drive is known to be local-only: it only cleans up', async () => {
  db.messageMigrationPending.mockReturnValue(true);
  db.migrateMessagesStep.mockResolvedValue(
    JSON.stringify({ done: 3, total: 3, finished: true }),
  );

  const posted: Array<Record<string, unknown>> = [];
  const worker = {
    onmessage: null as unknown as (event: unknown) => void,
    postMessage: vi.fn((value: Record<string, unknown>) => posted.push(value)),
  };
  vi.stubGlobal('self', worker);
  await import('./client-db.worker.js');

  worker.onmessage({
    data: {
      id: 1,
      type: 'init',
      wasmUrl: 'data:text/javascript,export default async function() {}',
    },
  });
  await vi.waitFor(() => expect(posted.some(m => m.id === 1)).toBe(true));

  expect(db.migrateMessagesStep).toHaveBeenCalledWith(500, '[]');
});

it('moves the messages of AI chats after the group chats, with the same drives', async () => {
  db.aiChatMigrationPending.mockReturnValue(true);
  db.migrateAiChatsStep.mockResolvedValue(
    JSON.stringify({ done: 40, total: 40, finished: true }),
  );

  const posted: Array<Record<string, unknown>> = [];
  const worker = {
    onmessage: null as unknown as (event: unknown) => void,
    postMessage: vi.fn((value: Record<string, unknown>) => posted.push(value)),
  };
  vi.stubGlobal('self', worker);
  await import('./client-db.worker.js');

  worker.onmessage({
    data: {
      id: 1,
      type: 'init',
      wasmUrl: 'data:text/javascript,export default async function() {}',
      localOnlyDrives: ['did:ad:local'],
    },
  });
  await vi.waitFor(() => expect(posted.some(m => m.id === 1)).toBe(true));

  expect(db.migrateMessagesStep).not.toHaveBeenCalled();
  expect(db.migrateAiChatsStep).toHaveBeenCalledWith(
    500,
    JSON.stringify(['did:ad:local']),
  );
  expect(
    posted
      .filter(m => m.type === 'migration-progress')
      .map(m => [m.phase, m.done, m.total, m.finished]),
  ).toEqual([
    ['messages', 0, 0, false],
    ['messages', 40, 40, true],
  ]);
});
