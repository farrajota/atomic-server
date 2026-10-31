import {
  Resource,
  type Resource as ResourceType,
  type Store,
  type Version,
} from '@tomic/react';

const groupFormatter = new Intl.DateTimeFormat('default', {
  month: 'long',
  year: 'numeric',
});

/** Group versions by month for the history UI. */
export function groupVersionsByMonth(
  versions: Version[],
): Record<string, Version[]> {
  return versions.reduceRight(
    (acc, version) => {
      const createdDate = new Date(version.timestamp);
      const groupKey = groupFormatter.format(createdDate);
      const group = acc[groupKey] ?? [];

      return {
        ...acc,
        [groupKey]: [...group, version],
      };
    },
    {} as Record<string, Version[]>,
  );
}

/** Build an isolated Resource whose native Loro state is the selected version. */
export function resourceAtVersion(
  source: ResourceType,
  version: Version,
  store: Store,
): ResourceType {
  const historical = new Resource(source.subject);
  historical.setStore(store);
  const doc = source.getLoroDoc();

  if (!doc || version.frontiers.length === 0) {
    historical.applyHydratedValues(version.propvals.entries());

    return historical;
  }

  const snapshot = doc.forkAt(version.frontiers).export({ mode: 'snapshot' });
  const imported = historical.importLoroUpdate(snapshot, true);

  if (!imported.complete) {
    throw new Error(`Could not materialize history for ${source.subject}`);
  }

  return historical;
}
