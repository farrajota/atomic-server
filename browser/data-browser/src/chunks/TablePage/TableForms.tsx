import {
  core,
  forms,
  Resource,
  useCollection,
  useMemberFromCollection,
  useString,
  type CollectionItemProps,
} from '@tomic/react';
import { Row } from '@components/Row';
import { ResourceInline } from '@views/ResourceInline/ResourceInline';

const TABLE_DRIVE_PROPERTY = 'https://atomicdata.dev/properties/drive';

export function TableForms({ table }: { table: Resource }) {
  const [tableDrive] = useString(table, TABLE_DRIVE_PROPERTY);
  const { mapAll, collection } = useCollection(
    {
      property: core.properties.parent,
      value: table.subject,
      filters: [{ property: core.properties.isA, value: forms.classes.form }],
      drive: tableDrive,
    },
    { pageSize: 1000 },
  );

  if (!collection.totalMembers) return null;

  return (
    <Row wrapItems>
      <strong>Forms</strong>
      {mapAll(props => (
        <FormLink key={props.index} {...props} />
      ))}
    </Row>
  );
}

function FormLink({ collection, index }: CollectionItemProps) {
  const form = useMemberFromCollection(collection, index);

  return <ResourceInline subject={form.subject} />;
}
