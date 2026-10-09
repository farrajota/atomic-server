import React from 'react';
import { GettingStartedFlow } from './getting-started/GettingStartedFlow';
import { useSettings } from '../helpers/AppSettings';

type Props = {
  /** Canonical subject for the server home (used to refetch after sign-in). */
  subject: string;
};

/**
 * Full-screen entry when the server has nothing useful at `/`: sign-in for
 * visitors, or the existing recovery flow for an identity with no home here.
 */
export function RootWelcomeGate({ subject }: Props) {
  const { agent } = useSettings();

  return (
    <GettingStartedFlow
      subject={subject}
      initialStep={agent ? 'connect-device' : 'welcome'}
    />
  );
}
