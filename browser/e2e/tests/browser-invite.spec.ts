import type { WebSocketRoute } from '@playwright/test';
import { standaloneTest as test, expect } from './deployment-fixtures';
import { devDrive, FRONTEND_URL, topBarShareButton } from './test-utils';

// Relay only discovery and SDP in-process. Authentication, invite redemption,
// resource sync and persistence still travel over real WebRTC data channels.
// This runs against the production bundle without Vite source imports or SaaS.
test('joins an unhosted drive through its signed browser invitation', async ({
  browser,
}) => {
  test.setTimeout(90000);
  const rooms = new Map<string, Map<string, WebSocketRoute>>();
  const ownerContext = await browser.newContext({
    permissions: ['clipboard-write'],
  });
  const guestContext = await browser.newContext();
  const readerContext = await browser.newContext();

  for (const context of [ownerContext, guestContext, readerContext]) {
    // The app has no built-in signalling service; this suite chooses one, and
    // the route below answers it in-process.
    // Only on real origins: about:blank refuses localStorage and would report
    // a page error, which this suite fails on.
    await context.addInitScript(() => {
      if (!location.protocol.startsWith('http')) return;
      localStorage.setItem(
        'peer-signaling-url',
        'wss://signalling.invalid/webrtc-signal',
      );
    });
    await context.routeWebSocket('**/webrtc-signal', socket => {
      let room: Map<string, WebSocketRoute> | undefined;
      let peer: string | undefined;
      socket.onMessage(raw => {
        const message = JSON.parse(String(raw));

        if (message.type === 'join') {
          room = rooms.get(message.room) ?? new Map();
          rooms.set(message.room, room);
          peer = message.peer;
          socket.send(
            JSON.stringify({
              type: 'joined',
              peers: [...room.keys()],
              iceServers: [],
            }),
          );
          for (const other of room.values())
            other.send(JSON.stringify({ type: 'peer', peer }));
          room.set(peer!, socket);
        } else if (room && peer && ['offer', 'answer'].includes(message.type)) {
          room.get(message.to)?.send(
            JSON.stringify({
              type: message.type,
              from: peer,
              sdp: message.sdp,
            }),
          );
        }
      });
      socket.onClose(() => {
        if (!room || !peer) return;
        room.delete(peer);
        for (const other of room.values())
          other.send(JSON.stringify({ type: 'left', peer }));
      });
    });
  }

  const owner = await ownerContext.newPage();
  const guest = await guestContext.newPage();
  const reader = await readerContext.newPage();

  try {
    await devDrive(owner);
    await devDrive(guest);
    await devDrive(reader);
    const [ownerHome, writerHome, readerHome] = await Promise.all([
      owner.evaluate(() => window.store.getDrive()!),
      guest.evaluate(() => window.store.getDrive()!),
      reader.evaluate(() => window.store.getDrive()!),
    ]);
    const [ownerAgent, writerAgent, readerAgent] = await Promise.all([
      owner.evaluate(() => window.store.getAgent()!.subject),
      guest.evaluate(() => window.store.getAgent()!.subject),
      reader.evaluate(() => window.store.getAgent()!.subject),
    ]);
    expect(new Set([ownerAgent, writerAgent, readerAgent]).size).toBe(3);
    const drive = await owner.evaluate(async () => {
      const resource = await window.store.createDrive(
        'Browser invite acceptance',
        {
          personal: false,
          localOnly: true,
        },
      );

      return resource.subject;
    });
    expect(drive).not.toBe(ownerHome);
    expect(drive).not.toBe(writerHome);
    expect(drive).not.toBe(readerHome);
    const article = await owner.evaluate(async driveSubject => {
      const resource = await window.store.newResource({
        parent: driveSubject,
        isA: 'https://atomicdata.dev/classes/Article',
        propVals: {
          'https://atomicdata.dev/properties/name': 'Shared notes',
          'https://atomicdata.dev/properties/description':
            'Content available to invited readers',
        },
      });
      await resource.save();

      return resource.subject;
    }, drive);
    await owner.goto(
      `${FRONTEND_URL}/app/show?subject=${encodeURIComponent(drive)}`,
    );
    await topBarShareButton(owner).click();
    await owner.getByLabel('Full name', { exact: true }).fill('Drive Owner');
    await owner
      .getByRole('button', { name: 'Save and continue', exact: true })
      .click();
    await owner
      .getByLabel('Role for people who join with the link')
      .selectOption('write');
    await owner
      .getByRole('button', { name: 'Copy invite link', exact: true })
      .click();
    const code = owner.locator('[data-invite-link]');
    await expect(code).toHaveAttribute('data-invite-link', /token=/);
    const invitation = new URL(
      (await code.getAttribute('data-invite-link'))!,
    ).searchParams.get('token')!;
    const inviteRequests: string[] = [];
    guest.on('request', request => {
      if (new URL(request.url()).pathname === '/invites')
        inviteRequests.push(request.method());
    });
    await guest.goto(
      `${FRONTEND_URL}/app/invite?${new URLSearchParams({ token: invitation })}`,
    );
    await expect(
      guest.getByRole('heading', { name: "You're invited to edit this drive" }),
    ).toBeVisible();
    await guest
      .getByRole('button', { name: 'Join drive', exact: true })
      .click();
    await expect(
      guest.getByRole('button', { name: 'Open drive', exact: true }),
    ).toBeVisible({ timeout: 45000 });
    expect(inviteRequests).toEqual([]);
    await guest
      .getByRole('button', { name: 'Open drive', exact: true })
      .click();
    await expect(guest).toHaveURL(/\/app\/show\?subject=/);
    await expect
      .poll(() =>
        guest.evaluate(async driveSubject => {
          const resource = window.store?.resources.get(driveSubject);

          return {
            ready: resource?.isReady(),
            name: resource?.get('https://atomicdata.dev/properties/name'),
            writable: (
              await resource?.canWrite(window.store?.getAgent()?.subject)
            )?.[0],
          };
        }, drive),
      )
      .toEqual({
        ready: true,
        name: 'Browser invite acceptance',
        writable: true,
      });
    // These checks cover UI and client permission state, not server authorization.
    await expect(guest.getByTestId('current-drive-title')).toHaveText(
      'Browser invite acceptance',
    );
    const articleURL = `${FRONTEND_URL}/app/show?subject=${encodeURIComponent(article)}`;
    await guest.goto(articleURL);
    await expect(guest.getByTestId('editable-title')).toBeVisible({
      timeout: 45_000,
    });
    await expect(
      guest.getByText('Content available to invited readers', { exact: true }),
    ).toBeVisible();
    await expect(guest.getByTitle('Edit content')).toBeVisible();

    await owner
      .getByLabel('Role for people who join with the link')
      .selectOption('read');
    await owner
      .getByRole('button', { name: 'Copy invite link', exact: true })
      .click();
    await expect(code).toHaveAttribute('data-invite-link', /token=/);
    const readerInvitation = new URL(
      (await code.getAttribute('data-invite-link'))!,
    ).searchParams.get('token')!;

    await reader.goto(
      `${FRONTEND_URL}/app/invite?${new URLSearchParams({ token: readerInvitation })}`,
    );
    await expect(
      reader.getByRole('heading', { name: /You're invited to/ }),
    ).toBeVisible();
    await reader
      .getByRole('button', { name: 'Join drive', exact: true })
      .click();
    await expect(
      reader.getByRole('button', { name: 'Open drive', exact: true }),
    ).toBeVisible({ timeout: 45_000 });
    await reader
      .getByRole('button', { name: 'Open drive', exact: true })
      .click();
    await expect(reader).toHaveURL(/\/app\/show\?subject=/);
    await expect(reader.getByTestId('current-drive-title')).toHaveText(
      'Browser invite acceptance',
      { timeout: 45_000 },
    );
    await expect
      .poll(() =>
        reader.evaluate(async driveSubject => {
          const resource = window.store?.resources.get(driveSubject);

          return {
            ready: resource?.isReady(),
            name: resource?.get('https://atomicdata.dev/properties/name'),
            writable: (
              await resource?.canWrite(window.store?.getAgent()?.subject)
            )?.[0],
          };
        }, drive),
      )
      .toEqual({
        ready: true,
        name: 'Browser invite acceptance',
        writable: false,
      });

    await reader.goto(articleURL);
    await expect(reader.getByTestId('editable-title')).toBeVisible({
      timeout: 45_000,
    });
    await expect(
      reader.getByText('Content available to invited readers', { exact: true }),
    ).toBeVisible();
    await expect(reader.getByTitle('Edit content')).toHaveCount(0);
  } finally {
    await ownerContext.close();
    await guestContext.close();
    await readerContext.close();
  }
});
