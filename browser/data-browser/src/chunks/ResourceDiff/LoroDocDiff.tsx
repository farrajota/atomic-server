import type { FC } from 'react';
import { useStore, type Resource } from '@tomic/react';
import { styled } from 'styled-components';
import { ReadOnlyDocumentContent } from '@chunks/RTE/ReadOnlyDocumentContent';

interface LoroDocDiffProps {
  oldResource?: Resource;
  newResource: Resource;
}

export const LoroDocDiff: FC<LoroDocDiffProps> = ({
  oldResource,
  newResource,
}) => {
  const store = useStore();

  return (
    <DiffGrid $hasBefore={oldResource !== undefined}>
      {oldResource && (
        <Panel aria-label='Before document body'>
          <Heading>Before</Heading>
          <ReadOnlyDocumentContent resource={oldResource} store={store} />
        </Panel>
      )}
      {oldResource ? (
        <Panel aria-label='After document body'>
          <Heading>After</Heading>
          <ReadOnlyDocumentContent resource={newResource} store={store} />
        </Panel>
      ) : (
        <Panel aria-label='Added document body'>
          <Heading>Added</Heading>
          <ReadOnlyDocumentContent resource={newResource} store={store} />
        </Panel>
      )}
    </DiffGrid>
  );
};

const DiffGrid = styled.div<{ $hasBefore: boolean }>`
  display: grid;
  grid-template-columns: ${p =>
    p.$hasBefore ? 'repeat(2, minmax(0, 1fr))' : '1fr'};
  gap: 1rem;

  @media (max-width: 700px) {
    grid-template-columns: 1fr;
  }
`;

const Panel = styled.section`
  min-width: 0;
  padding: 0.75rem;
  border: 1px solid ${p => p.theme.colors.bg2};
  border-radius: ${p => p.theme.radius};
`;

const Heading = styled.h3`
  margin: 0 0 0.5rem;
  color: ${p => p.theme.colors.textLight};
  font-size: 0.9rem;
`;
