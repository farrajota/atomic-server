import * as React from 'react';
import {
  Client,
  core,
  dataBrowser,
  server,
  useArray,
  useResource,
  useStore,
  useString,
  useSubject,
} from '@tomic/react';
import ResourcePage from '../views/ResourcePage';
import { Search } from './Search/SearchRoute';
import { About } from './AboutRoute';
import { createRoute, useLocation, useNavigate } from '@tanstack/react-router';
import { appRoute } from './RootRoutes';
import { pathNames, paths } from './paths';
import { useSettings } from '../helpers/AppSettings';
import { isOriginWithoutNode } from '../helpers/originNode';
import { isDriveSignInError } from '../helpers/isDriveSignInError';
import { openPrivateHome } from '../helpers/openPrivateHome';
import { privateHomeNudge } from '../helpers/privateHomeNudge';
import { selectReadableDrive } from '../helpers/readableDrive';
import { addRecentResource } from '../helpers/recentResources';

export type ShowRouteSearch = {
  subject: string;
  /** Explicit workspace selection, as used by the portal Open action. */
  drive?: string;
  /**
   * The active View of a Table, so switching tabs is linkable and lands in
   * browser history. Absent = the table's default view.
   */
  view?: string;
  editColumn?: string;
};

export const ShowRoute = createRoute({
  path: pathNames.show,
  component: () => <ShowComponent />,
  getParentRoute: () => appRoute,
  validateSearch: (search): ShowRouteSearch => ({
    subject: (search.subject as string) ?? '',
    drive:
      typeof search.drive === 'string' && Client.isValidSubject(search.drive)
        ? search.drive
        : undefined,
    view: (search.view as string) || undefined,
    editColumn: (search.editColumn as string) || undefined,
  }),
});

/** Renders either the Welcome page, an Individual resource, or search results. */
export const ShowComponent: React.FunctionComponent = () => {
  // Value shown in navbar, after Submitting
  const subject = ShowRoute.useSearch({ select: state => state.subject });
  const requestedDrive = ShowRoute.useSearch({ select: state => state.drive });
  const view = ShowRoute.useSearch({ select: state => state.view });
  const { agent, baseURL, drive, setDrive } = useSettings();
  const store = useStore();
  const navigate = useNavigate();
  const location = useLocation();

  // A persisted identity can land here without running the welcome flow.
  // Hydrate/recover its own home before materializing it; other resources
  // retain their normal missing/permission handling.
  React.useEffect(() => {
    if (!agent || !subject) return;
    let cancelled = false;
    void openPrivateHome(store, subject)
      .then(result => {
        if (cancelled || !result) return;
        setDrive(subject);
        if (result === 'created') privateHomeNudge();
      })
      .catch(() => {
        // ResourcePage displays the read error and its recovery actions.
      });

    return () => {
      cancelled = true;
    };
  }, [agent, subject, store, setDrive]);

  // Signed out on an origin that runs no node: nothing can load until a
  // sign-in restores the data, so go straight to sign-in with the local route
  // and query as `next`, preserving the table view and workspace selection.
  const signInFirst =
    !agent &&
    Client.isValidSubject(subject) &&
    isOriginWithoutNode(store.getServerUrl());

  // A drive this visitor cannot read is redirected to the sign-in step by
  // ErrorPage. That redirect and the consumption below navigate in the same
  // commit, and the later one wins: consuming first left the visitor on the
  // error page for good.
  const resource = useResource(
    Client.isValidSubject(subject) ? subject : undefined,
  );
  const signInRedirect =
    Client.isValidSubject(subject) &&
    isDriveSignInError(resource, agent, baseURL, {
      originWithoutNode: isOriginWithoutNode(store.getServerUrl()),
    });

  // A table View is configuration; the person wants its rows. A link or mention
  // that points at the View itself opens the table with that view selected, the
  // same address the table's own tab uses.
  const [viewParent] = useString(resource, core.properties.parent);
  const isTableView = resource.hasClasses(dataBrowser.classes.view);
  const [containingDrive] = useSubject(
    resource,
    'https://atomicdata.dev/properties/drive',
  );
  const [resourceClasses] = useArray(resource, core.properties.isA);
  const readableWorkspace =
    containingDrive ??
    (resourceClasses.includes(server.classes.drive) ? subject : undefined);

  // Startup adoption cannot read a private link before sign-in. A local
  // return path bypasses personal-home recovery, so adopt its existing
  // readable workspace here once the restored identity can read the resource.
  // Do not replace an established workspace or enroll a personal drive.
  React.useEffect(() => {
    if (
      !agent ||
      (drive && drive !== baseURL) ||
      requestedDrive ||
      signInFirst ||
      signInRedirect ||
      resource.error ||
      !readableWorkspace
    ) {
      return;
    }

    let cancelled = false;
    void selectReadableDrive(
      store,
      readableWorkspace,
      setDrive,
      () => !cancelled,
    );

    return () => {
      cancelled = true;
    };
  }, [
    agent,
    drive,
    baseURL,
    requestedDrive,
    signInFirst,
    signInRedirect,
    resource.error,
    readableWorkspace,
    store,
    setDrive,
  ]);

  React.useEffect(() => {
    if (!isTableView || !viewParent || view) {
      return;
    }

    navigate({
      to: paths.show,
      search: { subject: viewParent, view: subject },
      replace: true,
    });
  }, [isTableView, viewParent, view, subject, navigate]);

  React.useEffect(() => {
    if (
      signInFirst ||
      signInRedirect ||
      !requestedDrive ||
      requestedDrive !== subject
    ) {
      return;
    }

    let cancelled = false;
    void selectReadableDrive(
      store,
      requestedDrive,
      setDrive,
      () => !cancelled,
    ).then(selected => {
      if (!selected || cancelled) return;
      // Consume a successful selection so later manual switches stay put.
      navigate({ to: paths.show, search: { subject, view }, replace: true });
    });

    return () => {
      cancelled = true;
    };
  }, [
    signInFirst,
    signInRedirect,
    requestedDrive,
    subject,
    view,
    agent,
    store,
    setDrive,
    navigate,
  ]);

  React.useEffect(() => {
    if (!signInFirst) return;

    navigate({
      to: paths.welcome,
      search: {
        next: `${location.pathname}${location.searchStr}`,
        from_portal: undefined,
      },
      replace: true,
    });
  }, [signInFirst, location.pathname, location.searchStr, navigate]);

  // Feeds the "recent" list the document `@` menu shows before typing.
  React.useEffect(() => {
    if (signInFirst || !drive || !Client.isValidSubject(subject)) return;
    // Wait until the drive switch above has landed.
    if (requestedDrive && requestedDrive !== drive) return;

    addRecentResource(drive, subject);
  }, [signInFirst, drive, requestedDrive, subject]);

  if (signInFirst) {
    return null;
  }

  if (subject === undefined || subject === '') {
    return <About />;
  }

  if (Client.isValidSubject(subject)) {
    return (
      <ResourcePage
        key={subject}
        subject={subject}
        websiteVersion={
          view?.startsWith('website-version:') ? view.slice(16) : undefined
        }
      />
    );
  } else {
    return <Search />;
  }
};
