import { type Resource, isNotFound, isUnauthorized } from '@tomic/react';
import { isAtomicServerHome } from './isAtomicServerHome';

/**
 * True when the server home cannot be read. An anonymous visitor needs sign-in;
 * an existing identity with no home needs the existing device-recovery flow.
 */
export function isRootWelcomeResourceError(
  resource: Resource,
  baseURL: string,
): boolean {
  if (!resource.error) {
    return false;
  }

  return (
    isAtomicServerHome(resource.subject, baseURL) &&
    (isNotFound(resource.error) || isUnauthorized(resource.error))
  );
}
