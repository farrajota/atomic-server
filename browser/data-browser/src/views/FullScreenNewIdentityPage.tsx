import React from 'react';
import { useSettings } from '../helpers/AppSettings';
import { GettingStartedFlow } from './getting-started/GettingStartedFlow';

/**
 * On nodes that allow account creation, create an agent, set a name, then open
 * its private home. Owner-mode nodes instead show the existing-identity sign-in
 * step. App chrome (sidebar, top bar, AI panel) is hidden on this route via
 * NavWrapper.
 */
export const FullScreenNewIdentityPage: React.FC = () => {
  const { baseURL } = useSettings();

  return <GettingStartedFlow subject={baseURL} initialStep='create' />;
};
