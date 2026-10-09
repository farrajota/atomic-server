// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react';
import { AtomicError, ErrorType } from '@tomic/lib';
import { ThemeProvider } from 'styled-components';
import type * as TomicReact from '@tomic/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { buildTheme } from '../../styling';
import { SideBarDrive } from './SideBarDrive';

const state = vi.hoisted(() => ({
  driveResource: {
    stable: 'did:ad:missing-drive',
    error: new Error('Resource not found locally'),
  },
  store: {
    getResourceAncestry: vi.fn(async () => []),
  },
  settings: {
    drive: 'did:ad:missing-drive',
    agent: undefined as { subject: string } | undefined,
  },
}));

vi.mock('@tomic/react', async importOriginal => ({
  ...(await importOriginal<typeof TomicReact>()),
  useCanWrite: () => false,
  useChildren: () => ({ subjects: [], loading: false, total: 0 }),
  useResource: () => state.driveResource,
  useStore: () => state.store,
  useString: () => [undefined],
  useSubject: () => [undefined],
  useTitle: () => [undefined],
}));
vi.mock('../../helpers/AppSettings', () => ({
  useSettings: () => state.settings,
}));
vi.mock('../../helpers/useCurrentSubject', () => ({
  useCurrentSubject: () => [''],
}));
vi.mock('../../hooks/useIsPrivateDrive', () => ({
  useIsPrivateDrive: () => false,
}));
vi.mock('./useSidebarDnd', () => ({
  useSidebarDnd: () => ({
    handleDragStart: vi.fn(),
    handleDragEnd: vi.fn(),
    draggingResource: undefined,
    sensors: [],
    animateDrop: undefined,
    dndExplanation: '',
    announcements: {},
  }),
}));
vi.mock('./ResourceSideBar/ResourceSideBar', () => ({
  ResourceSideBar: () => null,
}));
vi.mock('./SideBarHeader', () => ({ SideBarHeader: 'div' }));
vi.mock('./DriveSwitcher', () => ({ DriveSwitcher: () => null }));
vi.mock('../ErrorLook', () => ({
  SimpleErrorBlock: ({ children }: { children: ReactNode }) => (
    <div>{children}</div>
  ),
}));
vi.mock('../Row', () => ({ Row: 'div' }));
vi.mock('../IconButton/IconButton', () => ({ IconButton: 'button' }));
vi.mock('../ScrollArea', () => ({ ScrollArea: 'div' }));
vi.mock('./ResourceSideBar/SidebarItemTitle', () => ({
  SidebarItemTitle: () => null,
}));
vi.mock('./ResourceSideBar/DropEdge', () => ({ DropEdge: () => null }));
vi.mock('./SideBarMoreRow', () => ({
  SIDEBAR_CHILD_LIMIT: 50,
  SideBarMoreRow: () => null,
}));
vi.mock('../Loader', () => ({ LoaderInline: () => null }));
vi.mock('../NewInstanceButton', () => ({ QuickCreateRow: () => null }));
vi.mock('../ResourceGlyph', () => ({ ResourceGlyph: () => null }));
vi.mock('@dnd-kit/core', () => ({
  closestCenter: vi.fn(),
  DndContext: ({ children }: { children: ReactNode }) => children,
  DragOverlay: () => null,
}));

function show() {
  render(
    <ThemeProvider theme={buildTheme(false, '#336699', false)}>
      <SideBarDrive
        onItemClick={() => undefined}
        onIsRearangingChange={() => undefined}
      />
    </ThemeProvider>,
  );
}

afterEach(() => cleanup());
beforeEach(() => {
  state.settings.agent = undefined;
  state.driveResource.error = new AtomicError(
    'Resource not found. DID Resource did:ad:missing-drive not found locally',
    ErrorType.NotFound,
  );
});

it('shows recovery guidance instead of a raw missing-DID message', () => {
  const missingDid = state.driveResource.error.message;
  show();

  expect(screen.queryByText(missingDid)).toBeNull();
  expect(screen.getByText(/not available on this device/i)).toBeTruthy();
  expect(screen.getByText(/sign in|connect/i)).toBeTruthy();
});

it('continues to show unrelated sidebar errors', () => {
  state.driveResource.error = new Error('The server is unreachable');
  show();

  expect(screen.getByText('The server is unreachable')).toBeTruthy();
});

it('keeps a private workspace understandable while reading public content', () => {
  state.settings.agent = { subject: 'did:ad:readonly' };
  state.driveResource.error = new AtomicError(
    'This resource is not publicly readable. Try signing in',
    ErrorType.Unauthorized,
  );
  show();

  expect(screen.queryByText(/^unauthorized$/i)).toBeNull();
  expect(screen.getByText('This account does not have access')).toBeTruthy();
  expect(
    screen.getByRole('button', { name: 'Use another account' }),
  ).toBeTruthy();
});

it('offers existing-identity sign-in for an anonymous private workspace', () => {
  state.driveResource.error = new AtomicError(
    'This resource is not publicly readable. Try signing in',
    ErrorType.Unauthorized,
  );
  show();

  expect(screen.queryByText(/^unauthorized$/i)).toBeNull();
  expect(screen.getByText('Sign in to open a workspace')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Sign in' })).toBeTruthy();
});
