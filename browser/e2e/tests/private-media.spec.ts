/**
 * File media served by the real server: private bytes stay private, the app
 * still shows them to their reader, and public bytes need no Agent.
 *
 * The server must run with `ATOMIC_REQUIRE_BLOB_AUTH=true`, as production
 * does: without it a content hash is the capability and every download is
 * 200 (see `docs/src/files.md`). With it, a content-addressed request is
 * answered only for an Agent who may read a resource referencing the blob;
 * anyone else gets 401. `scripts/e2e-server.sh` and `scripts/local-e2e.mjs`
 * start the server with it by default (`ATOMIC_REQUIRE_BLOB_AUTH=false` opts
 * out). The spec probes for it first and fails, rather than skips, on a
 * server without it: the private-media assertions would otherwise fail as if
 * the product leaked bytes.
 *
 * - Private File: `/download/files/<hash>` and `/download/atomic:blob:<hash>`
 *   are 401 anonymously and when signed by an Agent that cannot read the
 *   drive, and 200 when signed by the owner. A browser of the same Agent
 *   without a local copy previews the plain URL, authenticated by the session
 *   cookie rather than a signature.
 * - Public File: anonymous fetch is 200, and a browser without an Agent
 *   previews it through the plain URL.
 */
import { randomBytes } from 'node:crypto';
import { request as playwrightRequest, type Page } from '@playwright/test';
import { Agent, generateKeyPair, signRequest } from '@tomic/lib';
import { test, expect } from './fixtures';
import {
  before,
  FRONTEND_URL,
  getDevDriveSecret,
  SERVER_URL,
  signIn,
} from './test-utils';

const PUBLIC_AGENT = 'https://atomicdata.dev/agents/publicAgent';
const READ = 'https://atomicdata.dev/properties/read';
const DOWNLOAD_URL = 'https://atomicdata.dev/properties/downloadURL';

/**
 * The test Agent's own drive from `before()`, readable only by that Agent
 * unless `makePublic` adds the public Agent to its readers.
 */
async function ownDrive(page: Page, makePublic: boolean): Promise<string> {
  return page.evaluate(
    async ({ pub, readProp, publicAgent }) => {
      const drive = await window.store.getResource(window.store.getDrive()!);

      if (pub) {
        const current = (drive.get(readProp) as string[] | undefined) ?? [];
        await drive.set(readProp, [...current, publicAgent]);
        await drive.save();
      }

      return drive.subject;
    },
    { pub: makePublic, readProp: READ, publicAgent: PUBLIC_AGENT },
  );
}

async function uploadPng(
  page: Page,
  parent: string,
  name: string,
  color: string,
): Promise<{ subject: string; downloadUrl: string }> {
  return page.evaluate(
    async ({ parentSubject, fileName, fill, downloadProp }) => {
      const canvas = new OffscreenCanvas(32, 32);
      const ctx = canvas.getContext('2d')!;
      ctx.fillStyle = fill;
      ctx.fillRect(0, 0, 32, 32);
      const blob = await canvas.convertToBlob({ type: 'image/png' });
      const file = new File([await blob.arrayBuffer()], fileName, {
        type: 'image/png',
      });
      const [subject] = await window.store.uploadFiles([file], parentSubject);
      const resource = window.store.resources.get(subject)!;

      return {
        subject,
        downloadUrl: resource.get(downloadProp) as string,
      };
    },
    {
      parentSubject: parent,
      fileName: name,
      fill: color,
      downloadProp: DOWNLOAD_URL,
    },
  );
}

async function signedHeaders(url: string, secret: string) {
  return signRequest(url, Agent.fromSecret(secret, 'js'), {});
}

/** A valid Agent the server has never seen and no drive grants anything. */
async function strangerSecret(): Promise<string> {
  const { privateKey, publicKey } = await generateKeyPair();

  return btoa(
    JSON.stringify({ privateKey, subject: `did:ad:agent:${publicKey}` }),
  );
}

const showUrl = (subject: string) =>
  `${FRONTEND_URL}/app/show?subject=${encodeURIComponent(subject)}`;

/**
 * Fails unless the server gates content-addressed reads. A hash no resource
 * references is a blob nobody may read: with `ATOMIC_REQUIRE_BLOB_AUTH=true`
 * an anonymous request for it is refused with 401 before any lookup, while
 * without the flag the server looks the bytes up and answers 404 (or 200 when
 * it holds them).
 */
async function expectBlobAuthRequired(): Promise<void> {
  const unreferenced = `${SERVER_URL}/download/files/${randomBytes(32).toString('hex')}`;
  const api = await playwrightRequest.newContext();

  try {
    const status = (await api.get(unreferenced)).status();
    expect(
      status,
      `server not started with ATOMIC_REQUIRE_BLOB_AUTH=true: an anonymous request for an unreadable content-addressed blob got ${status}, not 401`,
    ).toBe(401);
  } finally {
    await api.dispose();
  }
}

test.describe('private vs public media', () => {
  test.beforeAll(expectBlobAuthRequired);
  test.beforeEach(before);

  test('a private image is denied anonymously and previewed with the session cookie', async ({
    page,
    browser,
  }) => {
    test.setTimeout(150_000);
    const secret = await getDevDriveSecret(page);
    const drive = await ownDrive(page, false);
    const { subject, downloadUrl } = await uploadPng(
      page,
      drive,
      'private.png',
      'red',
    );
    expect(downloadUrl).toMatch(/\/download\/files\/[0-9a-f]{64}$/);
    const hash = downloadUrl.split('/').pop()!;
    const blobUrl = `${new URL(downloadUrl).origin}/download/atomic:blob:${hash}`;

    const api = await playwrightRequest.newContext();

    try {
      // The bytes reach the server through blob sync after the upload returns.
      await expect
        .poll(
          async () =>
            (
              await api.get(downloadUrl, {
                headers: await signedHeaders(downloadUrl, secret),
              })
            ).status(),
          { timeout: 45_000 },
        )
        .toBe(200);
      expect((await api.get(downloadUrl)).status()).toBe(401);
      expect((await api.get(blobUrl)).status()).toBe(401);
      // A signature proves who asks, not that they may read the bytes.
      const stranger = await strangerSecret();

      for (const url of [downloadUrl, blobUrl]) {
        expect(
          (
            await api.get(url, {
              headers: await signedHeaders(url, stranger),
            })
          ).status(),
        ).toBe(401);
      }

      expect(
        (
          await api.get(blobUrl, {
            headers: await signedHeaders(blobUrl, secret),
          })
        ).status(),
      ).toBe(200);
    } finally {
      await api.dispose();
    }

    // The uploading browser previews its local copy; a second browser of the
    // same Agent with Local DB off has none and must ask the server. (With
    // Local DB on, the bytes would arrive over sync and show as `blob:` too.)
    const otherContext = await browser.newContext();

    try {
      const other = await otherContext.newPage();
      const downloads: { url: string; status: number; headers: string[] }[] =
        [];
      other.on('response', async response => {
        if (!response.url().startsWith(downloadUrl)) return;
        downloads.push({
          url: response.url(),
          status: response.status(),
          headers: Object.keys(await response.request().allHeaders()),
        });
      });
      await other.goto(`${FRONTEND_URL}/app/welcome`);
      await signIn(other, secret);
      // What the Sync page's Local DB toggle does, as in local-db-off-server-only.
      await other.evaluate(() =>
        localStorage.setItem('atomic-disable-client-db', '1'),
      );
      await other.goto(showUrl(subject));
      await expect
        .poll(() =>
          other.evaluate(() => !!window.store && !window.store.getClientDb()),
        )
        .toBe(true);

      const image = other.locator('[data-test="image-viewer"]').first();
      await expect(image).toHaveAttribute('src', downloadUrl, {
        timeout: 45_000,
      });
      await expect
        .poll(() => image.evaluate((img: HTMLImageElement) => img.naturalWidth))
        .toBe(32);
      const served = downloads.find(d => d.url === downloadUrl);
      expect(served?.status).toBe(200);
      expect(served?.headers).toContain('cookie');
      expect(served?.headers).not.toContain('x-atomic-signature');
      await other.screenshot({
        path: test.info().outputPath('private-image-preview.png'),
      });
    } finally {
      await otherContext.close();
    }
  });

  test('a public image is previewed through its plain URL without an Agent', async ({
    page,
    browser,
  }) => {
    const drive = await ownDrive(page, true);
    const { subject, downloadUrl } = await uploadPng(
      page,
      drive,
      'public.png',
      'blue',
    );

    const api = await playwrightRequest.newContext();

    try {
      await expect
        .poll(async () => (await api.get(downloadUrl)).status(), {
          timeout: 45_000,
        })
        .toBe(200);
    } finally {
      await api.dispose();
    }

    const anonContext = await browser.newContext();

    try {
      const anon = await anonContext.newPage();
      const download = anon.waitForResponse(
        response => response.url() === downloadUrl,
        { timeout: 45_000 },
      );
      await anon.goto(showUrl(subject));
      const image = anon.locator('[data-test="image-viewer"]').first();
      await expect(image).toHaveAttribute('src', downloadUrl, {
        timeout: 45_000,
      });
      const response = await download;
      expect(response.status()).toBe(200);
      expect(Object.keys(await response.request().allHeaders())).not.toContain(
        'x-atomic-signature',
      );
      await expect
        .poll(() => image.evaluate((img: HTMLImageElement) => img.naturalWidth))
        .toBe(32);
      await anon.screenshot({
        path: test.info().outputPath('public-image-anonymous-preview.png'),
      });
    } finally {
      await anonContext.close();
    }
  });
});
