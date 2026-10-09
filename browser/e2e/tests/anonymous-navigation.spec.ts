import { test, expect, type Page } from './fixtures';
import { FRONTEND_URL } from './test-utils';

const restrictedDestinations = [
  '/app',
  '/app/new?parentSubject=https%3A%2F%2Fnode.example%2Fdrive&view=table',
  '/app/new-drive?template=team',
  '/app/dev-drive',
  '/app/token',
  '/app/agent?return_to=agent',
  '/app/invite?token=synthetic-invite&return_to=%2Fapp%2Fshow%3Fsubject%3Dhttps%253A%252F%252Fnode.example%252Ftable',
] as const;

for (const destination of restrictedDestinations) {
  test(`anonymous navigation to ${destination} offers sign-in and preserves its destination`, async ({
    page,
  }) => {
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));

    // Unlike the protected app routes, dev-drive deliberately bootstraps a
    // fresh identity on Open fixtures. Exercise its owner-host restriction
    // here; the real Open bootstrap remains covered by the storage specs.
    if (destination === '/app/dev-drive') {
      await page.route('**/server', async route => {
        const response = await route.fetch();
        const metadata = await response.json();
        await route.fulfill({
          response,
          json: {
            ...metadata,
            'https://atomicdata.dev/properties/server/acceptsNewDrives': false,
            'https://atomicdata.dev/properties/server/canCreateDrives': false,
          },
        });
      });
    }

    await page.goto(new URL(destination, FRONTEND_URL).toString());

    await expect(page).toHaveURL(url => {
      const next = new URL(url).searchParams.get('next');

      return (
        new URL(url).pathname === '/app/welcome' &&
        new URL(url).searchParams.get('step') === 'signin' &&
        next === destination
      );
    });
    await expect(page.getByRole('button', { name: 'Sign in' })).toBeVisible();
    await expect(page.getByLabel('Agent secret')).toBeVisible();
    await expect(page).toHaveURL(url => {
      const current = new URL(url);

      return (
        current.pathname === '/app/welcome' &&
        current.searchParams.get('next') === destination
      );
    });
    expect(pageErrors).toEqual([]);
  });
}

test('anonymous appearance settings remain readable', async ({ page }) => {
  const destination = '/app/settings?q=appearance&section=accessibility';
  await page.goto(new URL(destination, FRONTEND_URL).toString());

  await expect(page).toHaveURL(new URL(destination, FRONTEND_URL).toString());
  await expect(page.getByRole('heading', { name: 'Settings' })).toBeVisible();
});

async function openSidebarBeforeNavigation(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem('sideBarOpen', JSON.stringify(true));
  });
}

test('an open stored mobile sidebar does not block the anonymous sign-in flow', async ({
  page,
}) => {
  await page.setViewportSize({ width: 412, height: 915 });
  await openSidebarBeforeNavigation(page);
  await page.goto(
    new URL('/app/agent?return_to=agent', FRONTEND_URL).toString(),
  );

  await expect(page.getByRole('button', { name: 'Sign in' })).toBeVisible();
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByLabel('Agent secret')).toBeVisible();
  await expect(
    page.getByText(/Resource not found|not found locally/i),
  ).toHaveCount(0);
});
