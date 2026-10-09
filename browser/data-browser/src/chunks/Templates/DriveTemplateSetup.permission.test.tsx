// @vitest-environment jsdom
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/react';
import type * as TomicReact from '@tomic/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type * as ButtonModule from '../../components/Button';
import type * as RowModule from '../../components/Row';
import type * as InputStylesModule from '../../components/forms/InputStyles';
import { DriveTemplateSetup } from './DriveTemplateSetup';

const state = vi.hoisted(() => ({
  canCreateDrives: false,
  agent: { subject: 'did:ad:owner' },
  createdDrive: { subject: 'did:ad:new-drive' },
  store: {
    getAgent: () => state.agent,
    getServerUrl: () => 'https://node.example',
    createDrive: vi.fn(async () => state.createdDrive),
    notifyResourceManuallyCreated: vi.fn(),
  },
  setDrive: vi.fn(),
  setSideBarLocked: vi.fn(),
}));

vi.mock('@tomic/react', async importOriginal => ({
  ...(await importOriginal<typeof TomicReact>()),
  useStore: () => state.store,
}));
vi.mock('../../helpers/AppSettings', () => ({
  useSettings: () => ({
    agent: state.agent,
    setDrive: state.setDrive,
    setSideBarLocked: state.setSideBarLocked,
  }),
}));
vi.mock('../../helpers/managedServer', () => ({
  fetchManagedInfo: async (
    _serverUrl: string,
    agent?: { subject: string },
  ) => ({
    managed: false,
    portalUrl: null,
    acceptsNewDrives: false,
    canCreateDrives: agent ? state.canCreateDrives : undefined,
  }),
  accountCreationTarget: (info: { acceptsNewDrives?: boolean }) =>
    info.acceptsNewDrives === false
      ? { kind: 'unavailable', reason: 'node-has-owner' }
      : { kind: 'local' },
}));
vi.mock('../../helpers/managed/cloudSync', () => ({
  getManagedPortalUrl: () => undefined,
}));
vi.mock('../../helpers/managed/session', () => ({
  getManagedAccount: async () => null,
}));
vi.mock('../../helpers/managed/reconcile', () => ({
  localAgentIsDisposable: async () => false,
}));
vi.mock('../../components/Card', () => ({ Card: 'div' }));
vi.mock('../../components/Button', async importOriginal => ({
  ...(await importOriginal<typeof ButtonModule>()),
  Button: ({ children, ...props }: { children: ReactNode }) => (
    <button {...props}>{children}</button>
  ),
}));
vi.mock('../../components/Row', async importOriginal => ({
  ...(await importOriginal<typeof RowModule>()),
  Column: 'div',
  Row: 'div',
}));
vi.mock('../../components/forms/Field', () => ({
  default: ({
    children,
    fieldId,
    label,
  }: {
    children: ReactNode;
    fieldId: string;
    label: string;
  }) => (
    <label htmlFor={fieldId}>
      {label}
      {children}
    </label>
  ),
}));
vi.mock('../../components/forms/InputStyles', async importOriginal => ({
  ...(await importOriginal<typeof InputStylesModule>()),
  InputStyled: 'input',
  InputWrapper: 'div',
}));
vi.mock('../../components/forms/Checkbox', () => ({ Checkbox: 'input' }));
vi.mock('./demoSession', () => ({
  readTemplateDemo: () => undefined,
  TEMPLATE_DEMO_KEY: 'template-demo',
}));
vi.mock('./instantiate', () => ({
  instantiateTemplate: vi.fn(),
  startTemplateDemo: vi.fn(),
}));
vi.mock('./keepTemplateDemo', () => ({ keepTemplateDemo: vi.fn() }));
vi.mock('./pendingTemplate', () => ({
  clearPendingTemplate: vi.fn(),
  savePendingTemplate: vi.fn(),
}));
vi.mock('./prepareTemplateDrive', () => ({
  prepareTemplateDrive: vi.fn(async () => undefined),
}));

function show(onCreated = vi.fn()) {
  window.history.replaceState(null, '', '/app/new-drive?blank=1');
  render(<DriveTemplateSetup onCreated={onCreated} />);

  return onCreated;
}

afterEach(() => cleanup());
beforeEach(() => {
  vi.clearAllMocks();
  state.canCreateDrives = false;
  state.agent = { subject: 'did:ad:owner' };
  state.store.createDrive.mockImplementation(async () => state.createdDrive);
});

it('does not expose drive creation when the signed-in identity is refused', async () => {
  show();

  expect(
    await screen.findByText(/does not allow this account to create drives/i),
  ).toBeTruthy();
  expect(screen.queryByLabelText('Drive name')).toBeNull();
  expect(screen.queryByRole('button', { name: 'Create drive' })).toBeNull();
  expect(state.store.createDrive).not.toHaveBeenCalled();
});

it('rechecks admission before creating a drive', async () => {
  state.canCreateDrives = true;
  show();
  await screen.findByLabelText('Drive name');
  state.canCreateDrives = false;

  fireEvent.click(screen.getByRole('button', { name: 'Create drive' }));

  expect(
    await screen.findByText(/does not allow this account to create drives/i),
  ).toBeTruthy();
  expect(state.store.createDrive).not.toHaveBeenCalled();
});

it('preserves explicit drive creation for an identity allowed by the server', async () => {
  state.canCreateDrives = true;
  const onCreated = show();
  await screen.findByLabelText('Drive name');

  fireEvent.click(screen.getByRole('button', { name: 'Create drive' }));

  await waitFor(() => expect(state.store.createDrive).toHaveBeenCalled());
  expect(state.store.createDrive).toHaveBeenCalledWith('My drive', {
    personal: false,
    localOnly: false,
  });
  expect(onCreated).toHaveBeenCalledWith(state.createdDrive);
});
