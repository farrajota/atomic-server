import { useStore, type Resource } from '@tomic/react';
import { styled } from 'styled-components';
import { ReadOnlyDocumentContent } from '@chunks/RTE/ReadOnlyDocumentContent';

interface VersionContentProps {
  resource: Resource;
}

/** Render the native Loro body at the selected historical version. */
export function VersionContent({ resource }: VersionContentProps) {
  const store = useStore();

  if (!resource.hasLoroBody()) return null;

  return (
    <Wrap>
      <Heading>Document body</Heading>
      <ReadOnlyDocumentContent resource={resource} store={store} />
    </Wrap>
  );
}

const Wrap = styled.div`
  display: flex;
  flex-direction: column;
  gap: 1rem;
  padding: 1rem;
  border-top: 1px solid ${p => p.theme.colors.bg2};
`;

const Heading = styled.div`
  font-weight: bold;
  color: ${p => p.theme.colors.textLight};
`;
