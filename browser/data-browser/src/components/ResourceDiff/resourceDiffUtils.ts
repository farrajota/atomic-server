import {
  commits,
  getLoroBodySignature,
  isJSONObject,
  type AtomicValue,
  type Resource,
} from '@tomic/react';

export interface AtomicDiff {
  oldResource?: Resource;
  newResource: Resource;
  changedProps: string[];
  bodyChanged: boolean;
}

function stableStringify(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(stableStringify).join(',')}]`;
  }

  if (value && typeof value === 'object') {
    return `{${Object.entries(value as Record<string, unknown>)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(
        ([key, nested]) => `${JSON.stringify(key)}:${stableStringify(nested)}`,
      )
      .join(',')}}`;
  }

  return JSON.stringify(value) ?? '';
}

export function isPropEqual(
  oldProp: AtomicValue,
  newProp: AtomicValue,
): boolean {
  if (oldProp instanceof Uint8Array && newProp instanceof Uint8Array) {
    if (oldProp.length !== newProp.length) return false;

    for (let i = 0; i < oldProp.length; i++) {
      if (oldProp[i] !== newProp[i]) return false;
    }

    return true;
  }

  if (oldProp instanceof Uint8Array || newProp instanceof Uint8Array) {
    return false;
  }

  if (
    (isJSONObject(oldProp) && isJSONObject(newProp)) ||
    (Array.isArray(oldProp) && Array.isArray(newProp))
  ) {
    return stableStringify(oldProp) === stableStringify(newProp);
  }

  return oldProp === newProp;
}

function getChangedProps(
  oldResource: Resource | undefined,
  newResource: Resource,
): string[] {
  const changedProps: string[] = [];

  if (!oldResource) {
    changedProps.push(
      ...newResource
        .getEntries()
        .map(([key]) => key)
        .filter(
          key =>
            key !== commits.properties.lastCommit &&
            key !== commits.properties.loroUpdate,
        ),
    );

    return changedProps;
  }

  for (const [key, value] of oldResource.getEntries()) {
    if (
      key === commits.properties.lastCommit ||
      key === commits.properties.loroUpdate
    ) {
      continue;
    }

    if (!isPropEqual(value, newResource.get(key))) {
      changedProps.push(key);
    }
  }

  for (const [key] of newResource.getEntries()) {
    if (
      key === commits.properties.lastCommit ||
      key === commits.properties.loroUpdate
    ) {
      continue;
    }

    if (oldResource.get(key) === undefined) {
      changedProps.push(key);
    }
  }

  return changedProps;
}

export function useResourceDiff(
  oldResource: Resource | undefined,
  newResource: Resource,
): AtomicDiff {
  const changedProps = getChangedProps(oldResource, newResource);
  const bodyChanged = oldResource
    ? getLoroBodySignature(oldResource.getLoroDoc()) !==
      getLoroBodySignature(newResource.getLoroDoc())
    : newResource.hasLoroBody();

  return {
    oldResource,
    newResource,
    changedProps,
    bodyChanged,
  };
}
