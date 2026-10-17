// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/react';
import type {
  ButtonHTMLAttributes,
  InputHTMLAttributes,
  ReactNode,
} from 'react';
import { CollectionBuilder, core, Datatype } from '@tomic/react';
import type * as TomicReact from '@tomic/react';
import type { Resource } from '@tomic/react';
import toast from 'react-hot-toast';
import { formTestFixture } from '../FormBuilder/formTestFixture';
import { CreateTableFormDialog } from './CreateTableFormDialog';

const { mockUseTableFormColumns, mockUseStore, mockNavigate } = vi.hoisted(
  () => ({
    mockUseTableFormColumns: vi.fn(),
    mockUseStore: vi.fn(),
    mockNavigate: vi.fn(),
  }),
);

vi.mock('@tomic/react', async importOriginal => {
  const actual = await importOriginal<typeof TomicReact>();

  return { ...actual, useStore: mockUseStore };
});
vi.mock('@tanstack/react-router', () => ({ useNavigate: () => mockNavigate }));
vi.mock('../FormBuilder/useTableFormColumns', () => ({
  useTableFormColumns: mockUseTableFormColumns,
}));
vi.mock('@components/Dialog', () => ({
  Dialog: ({ children }: { children?: ReactNode }) => <div>{children}</div>,
  DialogTitle: ({ children }: { children?: ReactNode }) => (
    <div>{children}</div>
  ),
  DialogContent: ({ children }: { children?: ReactNode }) => (
    <div>{children}</div>
  ),
  DialogActions: ({ children }: { children?: ReactNode }) => (
    <div>{children}</div>
  ),
  useDialog: () => [{}, vi.fn(), vi.fn()],
}));
vi.mock('@components/Button', () => ({
  Button: ({
    children,
    subtle: _subtle,
    ...props
  }: ButtonHTMLAttributes<HTMLButtonElement> & { subtle?: boolean }) => (
    <button {...props}>{children}</button>
  ),
}));
vi.mock('@components/Row', () => ({
  Column: ({ children }: { children?: ReactNode }) => <div>{children}</div>,
}));
vi.mock('@components/forms/Field', () => ({
  default: ({
    label,
    fieldId,
    children,
  }: {
    label: string;
    fieldId: string;
    children?: ReactNode;
  }) => (
    <label htmlFor={fieldId}>
      {label}
      {children}
    </label>
  ),
}));
vi.mock('@components/forms/InputStyles', () => ({
  InputStyled: (props: InputHTMLAttributes<HTMLInputElement>) => (
    <input {...props} />
  ),
  InputWrapper: ({ children }: { children?: ReactNode }) => (
    <div>{children}</div>
  ),
}));
vi.mock('@components/forms/Checkbox', () => ({
  Checkbox: ({
    checked,
    disabled,
    onChange,
  }: {
    checked: boolean;
    disabled?: boolean;
    onChange: (checked: boolean) => void;
  }) => (
    <input
      type='checkbox'
      checked={checked}
      disabled={disabled}
      onChange={event => onChange(event.target.checked)}
    />
  ),
  CheckboxLabel: ({ children }: { children?: ReactNode }) => (
    <label>{children}</label>
  ),
}));
vi.mock('@components/forms/FormServerWarning', () => ({
  FormServerWarning: () => null,
}));
vi.mock('react-hot-toast', () => ({ default: { error: vi.fn() } }));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  vi.restoreAllMocks();
});

function renderDialog(relationOnly = false) {
  const fixture = formTestFixture();
  const columns = relationOnly
    ? [
        fixture.make(
          {
            [core.properties.name]: 'Tags',
            [core.properties.datatype]: Datatype.RESOURCEARRAY,
            [core.properties.classtype]: 'https://example.com/classes/Tag',
          },
          'https://example.com/properties/tags',
        ),
      ]
    : [
        fixture.make(
          {
            [core.properties.name]: 'Description',
            [core.properties.datatype]: Datatype.JSON,
          },
          'https://example.com/properties/description',
        ),
        fixture.make(
          {
            [core.properties.name]: 'Title',
            [core.properties.datatype]: Datatype.STRING,
          },
          'https://example.com/properties/title',
        ),
      ];
  const requires = columns.map(column => column.subject);
  const dataClass = fixture.make(
    { [core.properties.requires]: requires },
    'https://example.com/classes/Article',
  );
  Object.assign(fixture.store, {
    getDrive: () => undefined,
    getServerUrl: () => 'https://example.com',
    resources: { get: (subject: string) => fixture.resources.get(subject) },
  });
  mockUseStore.mockReturnValue(fixture.store);
  mockUseTableFormColumns.mockReturnValue({
    dataClass,
    columns,
    requires,
    loading: false,
  });
  render(
    <CreateTableFormDialog
      table={{ subject: 'https://example.com/tables/articles' } as Resource}
      dataClassSubject={dataClass.subject}
      onClose={vi.fn()}
    />,
  );

  return { fixture, newResource: fixture.store.newResource, columns };
}

describe('CreateTableFormDialog required columns', () => {
  it('explains an unsupported required column and prevents form creation', () => {
    const { newResource } = renderDialog();

    expect(screen.getByLabelText(/Description \(required\)/)).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain(
      'Description are not supported in forms. Make these columns form-compatible in the table schema before creating a form.',
    );
    expect(
      screen
        .getByRole('button', { name: 'Create form' })
        .hasAttribute('disabled'),
    ).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: 'Create form' }));

    expect(newResource).not.toHaveBeenCalled();
  });

  it('keeps compatible required columns selected and non-editable', () => {
    renderDialog();

    const titleLabel = screen.getByText('Title (required)').closest('label');
    const checkbox = titleLabel?.querySelector('input[type="checkbox"]');

    expect((checkbox as HTMLInputElement | null)?.checked).toBe(true);
    expect((checkbox as HTMLInputElement | null)?.disabled).toBe(true);
  });

  it('reports an unavailable relation target before creating any form resources', async () => {
    const { newResource } = renderDialog(true);
    vi.spyOn(CollectionBuilder.prototype, 'buildAndFetch').mockResolvedValue({
      [Symbol.asyncIterator]: async function* () {},
    } as never);

    fireEvent.click(screen.getByRole('button', { name: 'Create form' }));

    await waitFor(() => {
      expect(toast.error).toHaveBeenCalledWith(
        'No table found for this relation column',
      );
    });
    expect(newResource).not.toHaveBeenCalled();
  });
});
