// @vitest-environment jsdom
import { act, cleanup, renderHook } from '@testing-library/react';
import { Agent } from '@tomic/lib';
import type * as TomicReact from '@tomic/react';
import type * as ManagedServer from '../helpers/managedServer';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { useDevDrive, DEV_DRIVE_AGENT_NAME } from './useDevDrive';

const state = vi.hoisted(() => ({
  info: vi.fn(),
  navigate: vi.fn(),
  settings: {
    setAgent: vi.fn(),
    setDrive: vi.fn(),
    setServer: vi.fn(),
  },
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
  useStore: () => state.store,
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

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubEnv('VITE_ATOMIC_SERVER_URL', 'https://node.example');
  window.history.replaceState(null, '', '/app/dev-drive?fixture=storage');
  localStorage.clear();
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
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
