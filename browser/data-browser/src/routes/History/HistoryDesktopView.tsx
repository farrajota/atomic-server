import { useMemo } from 'react';
import type { HistoryViewProps } from './HistoryViewProps';
import { styled } from 'styled-components';
import { Button } from '../../components/Button';
import { Card } from '../../components/Card';
import { Column, Row } from '../../components/Row';
import { Title } from '../../components/Title';
import { VersionTitle } from './VersionTitle';
import { VersionScroller } from './VersionScroller';
import {
  ResourceDiff,
  useResourceDiff,
} from '@components/ResourceDiff/ResourceDiff';
import { Tabs } from '@components/Tabs';
import { plural } from '@helpers/plural';
import { ResourceCardDefault } from '@views/Card/ResourceCard';
import { useStore } from '@tomic/react';
import { resourceAtVersion } from './versionHelpers';
import { VersionContent } from './VersionContent';

export function HistoryDesktopView({
  resource,
  groupedVersions,
  selectedVersion,
  olderVersion,
  isCurrentVersion,
  onNextVersion,
  onPreviousVersion,
  onSelectVersion,
  onVersionAccept,
  attribution,
}: HistoryViewProps) {
  const store = useStore();

  const selectedVersionResource = useMemo(
    () => resourceAtVersion(resource, selectedVersion, store),
    [store, resource, selectedVersion],
  );

  const olderVersionResource = useMemo(
    () =>
      olderVersion
        ? resourceAtVersion(resource, olderVersion, store)
        : undefined,
    [store, resource, olderVersion],
  );
  const diff = useResourceDiff(olderVersionResource, selectedVersionResource);
  const changesCountText = plural(
    diff.changedProps.length + (diff.bodyChanged ? 1 : 0),
    ['1 Change', '# Changes'],
  );

  const tabs = [
    { label: changesCountText, value: 'changes' },
    { label: 'Resource', value: 'resource' },
  ];

  return (
    <>
      <CurrentItem>
        <Column fullHeight>
          <Title resource={resource} prefix='History of' link />
          <>
            <VersionTitle version={selectedVersion} attribution={attribution} />
            <StyledCard>
              <Tabs tabs={tabs} label='History'>
                <Card.Content>
                  <Tabs.Panel value='changes'>
                    <ResourceDiff diff={diff} />
                  </Tabs.Panel>
                  <Tabs.Panel value='resource'>
                    <>
                      <ResourceCardDefault resource={selectedVersionResource} />
                      <VersionContent resource={selectedVersionResource} />
                    </>
                  </Tabs.Panel>
                </Card.Content>
              </Tabs>
            </StyledCard>
            <Row>
              <Button onClick={onVersionAccept} disabled={isCurrentVersion}>
                Restore this version
              </Button>
            </Row>
          </>
        </Column>
      </CurrentItem>
      <VersionScroller
        persistSelection
        title={`History of ${resource.title}`}
        groupedVersions={groupedVersions}
        selectedVersion={selectedVersion}
        onSelectVersion={onSelectVersion}
        onNextItem={onNextVersion}
        onPreviousItem={onPreviousVersion}
      />
    </>
  );
}

const CurrentItem = styled.div`
  flex: 1;
  overflow-y: auto;
`;

const StyledCard = styled(Card)`
  flex: 1;
  overflow-y: auto;
`;
