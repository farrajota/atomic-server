// @vitest-environment jsdom
import React from 'react';
import { act, cleanup, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({
  agent: { subject: 'did:ad:agent-a' } as { subject: string } | undefined,
  findAccessibleDrives: vi.fn(),
  store: {
    getAgent: () => state.agent,
  },
}));

vi.mock('@tomic/react', async original => ({
  ...(await original<typeof import('@tomic/react')>()),
  useStore: () => state.store,
  useResources: () => new Map(),
}));
vi.mock('../../helpers/AppSettings', () => ({
  useSettings: () => ({
    agent: state.agent,
    drive: undefined,
    setDrive: vi.fn(),
  }),
}));
vi.mock('../../helpers/driveData', () => ({
  findAccessibleDrives: state.findAccessibleDrives,
}));
vi.mock('../../helpers/driveDisplayName', () => ({
  driveDisplayName: (_resource: unknown, subject: string) => subject,
}));
vi.mock('../../helpers/navigation', () => ({
  constructOpenURL: (subject: string) => `/app/show?subject=${subject}`,
}));
vi.mock('../../hooks/useDriveHistory', () => ({
  useDriveHistory: () => [[], vi.fn()],
}));
vi.mock('../../hooks/useSavedDrives', () => ({
  useSavedDrives: () => [[]],
}));
vi.mock('../../hooks/usePrivateDrive', () => ({
  usePrivateDrive: () => ({ privateDrive: undefined }),
}));
vi.mock('../../hooks/useDriveHostingStates', () => ({
  useDriveHostingStates: () => ({ states: () => null, refresh: vi.fn() }),
}));
vi.mock('../../hooks/useAccountDriveCatalog', () => ({
  useAccountDriveCatalog: () => ({ subjects: [], removed: [], entries: [] }),
}));
vi.mock('../../hooks/useNavigateWithTransition', () => ({
  useNavigateWithTransition: () => vi.fn(),
}));
vi.mock('../../hooks/useNewResourceUI', () => ({
  useNewResourceUI: () => vi.fn(),
}));
vi.mock('../../routes/paths', () => ({ paths: { agentSettings: '/app/agent' } }));
vi.mock('../Dropdown', () => ({
  DIVIDER: 'divider',
  DropdownMenu: ({ items }: { items: Array<{ id?: string; label?: string }> }) => (
    <div>
      {items
        .filter(item => item.id?.startsWith('did:'))
        .map(item => (
          <div key={item.id}>{item.label}</div>
        ))}
    </div>
  ),
}));

import { DriveSwitcher } from './DriveSwitcher';

beforeEach(() => {
  state.agent = { subject: 'did:ad:agent-a' };
  state.findAccessibleDrives.mockReset();
  state.findAccessibleDrives.mockResolvedValue([]);
});

afterEach(() => cleanup());

it('hides an Agent’s discovered Drives immediately when switching identities', async () => {
  const agentADiscovery = Promise.withResolvers<string[]>();
  const agentBDiscovery = Promise.withResolvers<string[]>();
  state.findAccessibleDrives.mockImplementation(() =>
    state.agent?.subject === 'did:ad:agent-a'
      ? agentADiscovery.promise
      : agentBDiscovery.promise,
  );
  const view = render(<DriveSwitcher />);

  await act(async () => agentADiscovery.resolve(['did:ad:shared-a']));
  expect(await screen.findByText('did:ad:shared-a')).toBeTruthy();

  state.agent = { subject: 'did:ad:agent-b' };
  view.rerender(<DriveSwitcher />);

  expect(screen.queryByText('did:ad:shared-a')).toBeNull();
  expect(state.findAccessibleDrives).toHaveBeenCalledTimes(2);

  await act(async () => agentBDiscovery.resolve([]));
  expect(screen.queryByText('did:ad:shared-a')).toBeNull();
});

it('renders without discovered Drives or discovery when initially signed out', () => {
  state.agent = undefined;

  const view = render(<DriveSwitcher />);

  expect(view.container.textContent).toBe('');
  expect(state.findAccessibleDrives).not.toHaveBeenCalled();
});

it('removes discovered Drives safely when signing out without anonymous discovery', async () => {
  const discovery = Promise.withResolvers<string[]>();
  state.findAccessibleDrives.mockReturnValue(discovery.promise);
  const view = render(<DriveSwitcher />);

  await act(async () => discovery.resolve(['did:ad:shared-a']));
  expect(await screen.findByText('did:ad:shared-a')).toBeTruthy();
  expect(state.findAccessibleDrives).toHaveBeenCalledTimes(1);

  state.agent = undefined;
  view.rerender(<DriveSwitcher />);

  expect(screen.queryByText('did:ad:shared-a')).toBeNull();
  expect(view.container.textContent).toBe('');
  expect(state.findAccessibleDrives).toHaveBeenCalledTimes(1);
});
