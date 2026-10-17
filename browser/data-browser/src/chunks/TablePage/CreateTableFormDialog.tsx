import { core, forms, Resource, useStore } from '@tomic/react';
import { useEffect, useId, useState } from 'react';
import { useNavigate } from '@tanstack/react-router';
import toast from 'react-hot-toast';
import { Button } from '@components/Button';
import { Column } from '@components/Row';
import {
  Dialog,
  DialogTitle,
  DialogContent,
  DialogActions,
  useDialog,
} from '@components/Dialog';
import Field from '@components/forms/Field';
import { FormServerWarning } from '@components/forms/FormServerWarning';
import { InputStyled, InputWrapper } from '@components/forms/InputStyles';
import { Checkbox, CheckboxLabel } from '@components/forms/Checkbox';
import { useTableFormColumns } from '../FormBuilder/useTableFormColumns';
import {
  compatibleColumnTypes,
  columnLabel,
  createMappedField,
  preflightMappedFields,
} from '../FormBuilder/tableColumns';
import { constructOpenURL } from '@helpers/navigation';

export function CreateTableFormDialog({
  table,
  dataClassSubject,
  onClose,
}: {
  table: Resource;
  dataClassSubject: string;
  onClose: () => void;
}) {
  const nameId = useId();
  const store = useStore();
  const navigate = useNavigate();
  const { dataClass, columns, requires, loading } =
    useTableFormColumns(dataClassSubject);
  const [name, setName] = useState('New form');
  const [excluded, setExcluded] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [dialogProps, show, hide] = useDialog({
    onCancel: onClose,
    onSuccess: onClose,
  });

  useEffect(() => show(), [show]);
  const columnSupported = new Map<string, boolean>();

  for (const property of columns) {
    columnSupported.set(
      property.subject,
      compatibleColumnTypes(store, dataClass.subject, property).length > 0,
    );
  }

  const unsupportedRequired = columns.filter(
    property =>
      requires.includes(property.subject) &&
      !columnSupported.get(property.subject),
  );

  const selectedColumns = columns.filter(
    property =>
      columnSupported.get(property.subject) &&
      (requires.includes(property.subject) ||
        !excluded.includes(property.subject)),
  );

  const create = async () => {
    if (loading || unsupportedRequired.length || !name.trim()) return;

    setBusy(true);

    try {
      const mappingPlans = await preflightMappedFields(
        store,
        dataClass,
        selectedColumns,
      );
      if (selectedColumns.some(property => !mappingPlans.has(property.subject)))
        throw new Error('Unable to resolve all selected columns');

      const form = await store.newResource({
        parent: table.subject,
        isA: forms.classes.form,
        propVals: {
          [core.properties.name]: name.trim(),
          [forms.properties.formDataClass]: dataClassSubject,
          [forms.properties.formTargetTable]: table.subject,
          [forms.properties.formPages]: [],
        },
      });
      await form.save();
      const page = await store.newResource({
        parent: form.subject,
        isA: forms.classes.formPage,
        propVals: {
          [core.properties.name]: 'Page 1',
          [forms.properties.formFields]: [],
        },
      });
      await page.save();
      // Link immediately so even an interrupted creation leaves a recoverable form.
      await form.set(forms.properties.formPages, [page.subject]);
      await form.save();

      for (const property of selectedColumns) {
        const field = await createMappedField(
          store,
          page,
          dataClass,
          property,
          mappingPlans.get(property.subject),
        );
        await page.set(forms.properties.formFields, [
          ...page.getSubjects(forms.properties.formFields),
          field.subject,
        ]);
        await page.save();
      }

      store.notifyResourceManuallyCreated(form);
      hide();
      await navigate({ to: constructOpenURL(form.subject) });
    } catch (error) {
      toast.error((error as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Dialog {...dialogProps}>
        <DialogTitle>
          <h1>Create form from this table</h1>
        </DialogTitle>
        <DialogContent>
          <Column>
            <FormServerWarning />
            <Field label='Form name' fieldId={nameId} required>
              <InputWrapper>
                <InputStyled
                  id={nameId}
                  autoFocus
                  disabled={busy}
                  value={name}
                  onChange={e => setName(e.target.value)}
                />
              </InputWrapper>
            </Field>
            <p>
              Select the columns to include. Responses will be added to this
              table.
            </p>
            {unsupportedRequired.length > 0 && (
              <p role='alert'>
                Required columns{' '}
                {unsupportedRequired.map(columnLabel).join(', ')} are not
                supported in forms. Make these columns form-compatible in the
                table schema before creating a form.
              </p>
            )}
            {columns.map(property => {
              const supported = columnSupported.get(property.subject) ?? false;
              const required = requires.includes(property.subject);

              return (
                <CheckboxLabel key={property.subject}>
                  <Checkbox
                    disabled={!supported || required || busy}
                    checked={
                      supported &&
                      (required || !excluded.includes(property.subject))
                    }
                    onChange={checked => {
                      if (required) return;

                      setExcluded(previous =>
                        checked
                          ? previous.filter(s => s !== property.subject)
                          : [...previous, property.subject],
                      );
                    }}
                  />
                  {columnLabel(property)}
                  {required ? ' (required)' : ''}
                  {!supported ? ' — not supported in forms' : ''}
                </CheckboxLabel>
              );
            })}
          </Column>
        </DialogContent>
        <DialogActions>
          <Button subtle disabled={busy} onClick={() => hide()}>
            Cancel
          </Button>
          <Button
            disabled={
              busy || loading || !name.trim() || unsupportedRequired.length > 0
            }
            onClick={create}
          >
            Create form
          </Button>
        </DialogActions>
      </Dialog>
    </>
  );
}
