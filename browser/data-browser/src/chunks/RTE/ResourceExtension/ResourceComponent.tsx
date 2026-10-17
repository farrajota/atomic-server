import { lazy, Suspense } from 'react';
import { AtomicLink } from '@components/AtomicLink';
import { getIconForClass } from '@helpers/iconMap';
import type { ReactNodeViewProps } from '@tiptap/react';
import { NodeViewWrapper } from '@tiptap/react';
import { dataBrowser, useResource } from '@tomic/react';
import { styled } from 'styled-components';
import { TableRTE } from '../TableRTE';
import { RTENodeViewWrapper } from './RTENodeViewWrapper';
import { ErrorBoundary } from '@views/ErrorPage';
import { Spinner } from '@components/Spinner';

// The node schema is also imported by the historical read-only renderer. The
// full resource card imports its own rich-text extensions, so load that card
// only when an embedded resource node is actually rendered.
const ResourceCard = lazy(() => import('@views/Card/ResourceCard'));

export const ResourceComponent = (
  props: ReactNodeViewProps<HTMLDivElement>,
) => {
  const resource = useResource(props.node.attrs.subject);

  const [Component, wide] = resource.matchClass(
    {
      [dataBrowser.classes.table]: [TableRTE, true],
    },
    [ResourceCard, false],
  );

  return (
    <RTENodeViewWrapper wide={wide} key={props.node.attrs.subject}>
      <ErrorBoundary>
        <Suspense
          fallback={
            <span role='status' aria-label='Loading embedded resource'>
              <Spinner size='1.5rem' />
            </span>
          }
        >
          <Component
            subject={props.node.attrs.subject}
            highlight={props.selected}
          />
        </Suspense>
      </ErrorBoundary>
    </RTENodeViewWrapper>
  );
};

export const ResourceInlineComponent = (
  props: ReactNodeViewProps<HTMLAnchorElement>,
) => {
  const resource = useResource(props.node.attrs.subject);
  const Icon = getIconForClass(resource.getClasses()[0]);

  return (
    <NodeViewWrapper as='span'>
      <StyledAtomicLink clean subject={resource.subject}>
        <Icon />
        {resource.title}
      </StyledAtomicLink>
    </NodeViewWrapper>
  );
};

const StyledAtomicLink = styled(AtomicLink)`
  display: inline-flex;
  align-items: center;
  gap: 0.5ch;
  color: ${props => props.theme.colors.mainSelectedFg};
  background-color: ${props => props.theme.colors.mainSelectedBg};
  padding: 0rem 0.4rem;
  border-radius: ${props => props.theme.radius};
  border: 1px solid ${props => props.theme.colors.mainSelectedFg};
  user-select: none;
`;
