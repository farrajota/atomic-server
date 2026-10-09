// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type { ReactNode } from 'react';
import { ThemeProvider } from 'styled-components';
import type * as TomicReact from '@tomic/react';
import { buildTheme } from '../../styling';
import { NewRoute } from './NewRoute';

const state = vi.hoisted(() => ({
  canWrite: false,
  search: {
    classSubject: undefined as string | undefined,
    parent: undefined as string | undefined,
    parentSubject: 'https://example.com/drives/shared',
    newSubject: undefined as string | undefined,
  },
  parentResource: {
    subject: 'https://example.com/drives/shared',
    stable: 'https://example.com/drives/shared',
  },
  store: {
    getServerUrl: () => 'https://example.com',
    getResourceLoading: () => ({ get: () => undefined }),
  },
  navigate: vi.fn(),
  showNewResourceUI: vi.fn(),
  askAI: vi.fn(),
}));

vi.mock('@tanstack/react-router', () => ({
  createRoute: (options: Record<string, unknown>) => ({
    ...options,
    options,
    useSearch: () => state.search,
  }),
}));
vi.mock('@tomic/react', async importOriginal => ({
  ...(await importOriginal<typeof TomicReact>()),
  useCanWrite: () => state.canWrite,
  useResource: () => state.parentResource,
  useServerSearch: () => ({ results: [], loading: false, error: undefined }),
  useStore: () => state.store,
}));
vi.mock('../RootRoutes', () => ({
  appRoute: {},
  RequireAgent: ({ children }: { children: ReactNode }) => children,
}));
vi.mock('../../helpers/AppSettings', () => ({
  useSettings: () => ({
    drive: 'https://example.com/drives/home',
    hideTemplates: true,
  }),
}));
vi.mock('../../components/Containers', () => ({
  ContainerNarrow: 'div',
  ContainerWide: 'div',
}));
vi.mock('../../components/Main', () => ({ Main: 'main' }));
vi.mock('../../components/Button', () => ({ Button: 'button' }));
vi.mock('../../components/Row', () => ({ Column: 'div', Row: 'div' }));
vi.mock('../../components/forms/InputStyles', () => ({
  InputStyled: 'input',
  InputWrapper: 'div',
  TextAreaStyled: 'textarea',
}));
vi.mock('../../components/forms/ResourceSelector', () => ({
  ResourceSelector: () => null,
}));
vi.mock('../../components/forms/FileDropzone/FileDropzoneInput', () => ({
  FileDropzoneInput: 'div',
}));
vi.mock('../../components/forms/NewForm/NewFormPage', () => ({
  NewFormFullPage: () => null,
}));
vi.mock('../../components/forms/NewForm/useNewResourceUI', () => ({
  useNewResourceUI: () => state.showNewResourceUI,
}));
vi.mock('../../views/ResourceInline', () => ({ ResourceInline: 'span' }));
vi.mock('../../hooks/useNavigateWithTransition', () => ({
  useNavigateWithTransition: () => state.navigate,
}));
vi.mock('../../components/AI/AISidebarContext', () => ({
  useAISidebar: () => ({ askAI: state.askAI }),
}));
vi.mock('../../components/AI/AISettingsContext', () => ({
  useAISettings: () => ({ enableAI: false }),
}));
vi.mock('../../hooks/useNewActionDiscovered', () => ({
  useMarkNewActionDiscovered: () => undefined,
}));
vi.mock('../../components/Template/ApplyTemplateDialog', () => ({
  ApplyTemplateDialog: () => null,
}));
vi.mock('../../helpers/iconMap', () => ({ getIconForClass: () => 'span' }));
vi.mock('./creationAssistant', () => ({ creationAssistantAsk: () => '' }));
vi.mock('./creationCatalog', () => ({
  AI_BUILD_SUGGESTIONS: [],
  BASIC_CREATIONS: [
    {
      subject: 'https://example.com/classes/Document',
      title: 'Document',
      description: 'Create a document.',
    },
  ],
  CREATION_PAGE_TEMPLATES: [],
  CREATION_TABLE_TEMPLATES: [],
  DRIVE_CREATIONS: [],
  isUntouchedSuggestion: () => false,
  matchesCreationSearch: () => true,
}));

function show() {
  const Route = NewRoute.options.component;
  if (!Route) throw new Error('NewRoute has no component');

  render(
    <ThemeProvider theme={buildTheme(false, '#336699', false)}>
      <Route />
    </ThemeProvider>,
  );
}

afterEach(() => cleanup());
beforeEach(() => {
  vi.clearAllMocks();
  state.canWrite = false;
});

it('explains a readonly destination without exposing create actions', () => {
  show();

  expect(screen.getByText(/read.only/i)).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Document' })).toBeNull();
  expect(screen.queryByRole('button', { name: /upload/i })).toBeNull();
});

it('keeps resource creation available when the destination grants write access', () => {
  state.canWrite = true;
  show();

  expect(screen.getByRole('button', { name: 'Document' })).toBeTruthy();
});
