// @vitest-environment jsdom
// @wc-ignore-file
import React from 'react';
import type * as TomicReact from '@tomic/react';
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({
  location: { pathname: '/', searchStr: '' },
  drive: 'did:ad:unreadable-last-used',
  privateDrive: 'did:ad:private-home',
  accessibleDrives: ['did:ad:shared'],
  homeDrive: undefined as string | undefined,
  readable: { 'did:ad:shared': true } as Record<string, boolean>,
  navigate: vi.fn(),
  settingsSetDrive: vi.fn(),
  settings: {
    baseURL: 'https://node.example',
    agent: { subject: 'did:ad:agent:service' },
    drive: 'did:ad:unreadable-last-used',
    setDrive: vi.fn(),
  },
  store: {
    getServerUrl: () => 'https://node.example',
  },
}));

vi.mock('../components/BrowserPeerWatcher', () => ({
  BrowserPeerWatcher: () => null,
}));
vi.mock('../Providers', () => ({
  Providers: ({ children }: React.PropsWithChildren) => children,
}));
vi.mock('../components/IdentityReconcileGate', () => ({
  IdentityReconcileGate: () => null,
}));
vi.mock('../components/DeviceLockWatcher', () => ({
  DeviceLockWatcher: () => null,
}));
vi.mock('../components/CloudVaultWatcher', () => ({
  CloudVaultWatcher: () => null,
}));
vi.mock('../components/EncryptionKeyPublisher', () => ({
  EncryptionKeyPublisher: () => null,
}));
vi.mock('../components/PairingLinkHandler', () => ({
  PairingLinkHandler: () => null,
}));
vi.mock('../components/pairing/PairingFlowProvider', () => ({
  PairingFlowProvider: ({ children }: React.PropsWithChildren) => children,
}));
vi.mock('@tanstack/react-router', () => ({
  createRootRoute: (options: unknown) => ({ options }),
  createRoute: (options: unknown) => ({ options }),
  Outlet: 'div',
  useLocation: () => state.location,
}));
vi.mock('@tomic/react', async original => ({
  ...(await original<typeof TomicReact>()),
  useStore: () => state.store,
}));
vi.mock('../helpers/AppSettings', () => ({
  useSettings: () => state.settings,
}));
vi.mock('../helpers/tauri', () => ({
  getLocalServerOrigin: () => 'http://localhost:9883',
  isRunningInTauri: () => false,
}));
vi.mock('../config', () => ({ isDev: () => false }));
vi.mock('../helpers/privateDrive', () => ({
  fetchPrivateDriveSubject: async () => state.privateDrive,
}));
vi.mock('../helpers/driveData', () => ({
  deviceHasDriveData: async (_store: unknown, subject: string) =>
    state.readable[subject] === true,
  findAccessibleDrives: async () => [...state.accessibleDrives].sort(),
}));
vi.mock('../helpers/managedServer', () => ({
  fetchManagedInfo: async () => ({ acceptsNewDrives: false }),
}));
vi.mock('../helpers/withDeadline', () => ({
  withDeadline: async (
    work: Promise<unknown>,
    _ms: number,
    fallback: unknown,
  ) => work.catch(() => fallback),
}));
vi.mock('../helpers/homeDrive', () => ({
  getHomeDrive: () => state.homeDrive,
}));
vi.mock('../hooks/useNavigateWithTransition', () => ({
  useNavigateWithTransition: () => state.navigate,
}));
vi.mock('../views/ResourcePage', () => ({
  default: ({ subject }: { subject: string }) => (
    <div data-testid='resource-page'>{subject}</div>
  ),
}));

import { topRoute } from './RootRoutes';

function renderTopRoute() {
  const component = topRoute.options.component;

  if (!component) throw new Error('Top route component is missing');
  render(React.createElement(component));
}

beforeEach(() => {
  vi.clearAllMocks();
  state.homeDrive = undefined;
  state.location = { pathname: '/', searchStr: '' };
  state.drive = 'did:ad:unreadable-last-used';
  state.privateDrive = 'did:ad:private-home';
  state.accessibleDrives = ['did:ad:shared'];
  state.readable = { 'did:ad:shared': true };
  state.settings.baseURL = 'https://node.example';
  state.settings.drive = state.drive;
  state.settings.setDrive = state.settingsSetDrive;
});

afterEach(() => cleanup());

it('ignores unreadable last-used and private targets and opens the readable shared Drive', async () => {
  renderTopRoute();

  await waitFor(() => {
    expect(state.navigate).toHaveBeenCalledWith({
      to: '/app/show?subject=did%3Aad%3Ashared',
      replace: true,
    });
  });
  expect(state.settingsSetDrive).toHaveBeenCalledWith('did:ad:shared');
});

it('keeps a readable last-used Drive ahead of other accessible Drives', async () => {
  state.readable = {
    'did:ad:shared': true,
    'did:ad:unreadable-last-used': true,
  };
  renderTopRoute();

  await waitFor(() => {
    expect(state.navigate).toHaveBeenCalledWith({
      to: '/app/show?subject=did%3Aad%3Aunreadable-last-used',
      replace: true,
    });
  });
  expect(state.settingsSetDrive).not.toHaveBeenCalled();
});

it('opens the first of multiple readable Drives in stable subject order', async () => {
  state.accessibleDrives = ['did:ad:shared-z', 'did:ad:shared-a'];
  state.readable = {
    'did:ad:shared-z': true,
    'did:ad:shared-a': true,
  };
  renderTopRoute();

  await waitFor(() => {
    expect(state.navigate).toHaveBeenCalledWith({
      to: '/app/show?subject=did%3Aad%3Ashared-a',
      replace: true,
    });
  });
  expect(state.settingsSetDrive).toHaveBeenCalledWith('did:ad:shared-a');
});

it('ignores persisted and discovered server-root sentinels before opening a shared Drive', async () => {
  const rootSubject = `${window.location.origin}/`;
  const sharedDrive = 'https://z.example/drive/shared';
  state.settings.baseURL = window.location.origin;
  state.settings.drive = rootSubject;
  state.drive = rootSubject;
  state.privateDrive = rootSubject;
  state.accessibleDrives = [rootSubject, sharedDrive];
  state.readable = {
    [rootSubject]: true,
    [sharedDrive]: true,
  };
  renderTopRoute();

  await waitFor(() => {
    expect(state.settingsSetDrive).toHaveBeenCalledWith(sharedDrive);
  });
});

it('finishes root resolution when the only readable target is the server root', async () => {
  const rootSubject = `${window.location.origin}/`;
  state.settings.baseURL = window.location.origin;
  state.settings.drive = rootSubject;
  state.drive = rootSubject;
  state.privateDrive = rootSubject;
  state.accessibleDrives = [rootSubject];
  state.readable = { [rootSubject]: true };
  renderTopRoute();

  await waitFor(() => {
    expect(screen.getByTestId('resource-page').textContent).toContain(
      rootSubject,
    );
  });
  expect(state.navigate).not.toHaveBeenCalled();
});

it('renders a declared home Drive whose URL is the current bare root', async () => {
  state.homeDrive = `${window.location.origin}/`;
  state.settings.baseURL = window.location.origin;
  state.settings.drive = state.homeDrive;
  renderTopRoute();

  await waitFor(() => {
    expect(screen.getByTestId('resource-page').textContent).toContain(
      state.homeDrive!,
    );
  });
  expect(state.navigate).not.toHaveBeenCalled();
});
