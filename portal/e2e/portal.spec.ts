import { expect, test, type Page } from '@playwright/test';
import { mkdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

// Runs against `vite --mode mock`: every test starts from a fresh seed (new browser context).
const SHOTS =
  process.env.PORTAL_SHOTS ??
  '/private/tmp/claude-501/-Users-mlong-Documents-Development-magneton-work-remote-connection/2d817fe7-8397-456e-bde9-16b8335ba434/scratchpad/portal-shots';
mkdirSync(SHOTS, { recursive: true });

const shot = (page: Page, name: string) => page.screenshot({ path: join(SHOTS, `${name}.png`), fullPage: true });
const PASSWORD = 'correct-horse';

async function signIn(page: Page, email: string) {
  await page.goto('signin');
  await page.getByLabel('Work email').fill(email);
  await page.getByRole('button', { name: 'Continue' }).click();
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('heading', { name: /^Computers/ })).toBeVisible();
}

async function signOut(page: Page) {
  await page.getByRole('button', { name: /Account menu/ }).click();
  await page.getByRole('menuitem', { name: 'Sign out' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in to the admin portal' })).toBeVisible();
}

const tab = (page: Page, name: string) => page.getByRole('navigation', { name: 'Sections' }).getByRole('link', { name });

test('sign in with password', async ({ page }) => {
  await page.goto('');
  await expect(page).toHaveURL(/\/admin\/signin$/);
  await expect(page.getByRole('heading', { name: 'Sign in to the admin portal' })).toBeVisible();
  await shot(page, '01-signin-email');
  await page.getByLabel('Work email').fill('dana@northwind.test');
  await page.getByRole('button', { name: 'Continue' }).click();
  await expect(page.getByRole('button', { name: 'Email me a sign-in link' })).toBeVisible();
  await page.getByLabel('Password').fill('wrong');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('alert')).toContainText('don’t match');
  await shot(page, '02-signin-options');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('heading', { name: /^Computers/ })).toBeVisible();
  await expect(page.locator('tbody tr')).toHaveCount(18);
  await expect(page.getByRole('combobox', { name: 'Company' })).toHaveValue(/.+/);
  await shot(page, '03-computers');
});

test('SSO domain shows the SSO button', async ({ page }) => {
  await page.goto('signin');
  await page.getByLabel('Work email').fill('grace@harbor.test');
  await page.getByRole('button', { name: 'Continue' }).click();
  await expect(page.getByRole('link', { name: 'Continue with Harbor Accounting single sign-on' })).toBeVisible();
  await shot(page, '02b-signin-sso');
});

test('magic link and 6-digit code', async ({ page }) => {
  await page.goto('signin');
  await page.getByLabel('Work email').fill('sam@northwind.test');
  await page.getByRole('button', { name: 'Continue' }).click();
  await page.getByRole('button', { name: 'Email me a sign-in link' }).click();
  await expect(page.getByRole('heading', { name: 'Check your email' })).toBeVisible();
  const link = await page.evaluate(() => window.__rconnOutbox!.at(-1)!.link);
  await page.goto(link.replace(/^\/admin\//, ''));
  await expect(page.getByRole('heading', { name: 'Sign in to Rconn' })).toBeVisible();
  await shot(page, '04-magic-landing');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: /^Computers/ })).toBeVisible();

  // Opened in another browser: the code fallback.
  await signOut(page);
  await page.goto('magic?token=other-browser');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: 'Enter the code from the email' })).toBeVisible();
  await page.getByLabel('6-digit code').fill('246810');
  await shot(page, '05-magic-code');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: /^Computers/ })).toBeVisible();
});

test('rename and revoke a computer', async ({ page }) => {
  await signIn(page, 'dana@northwind.test');
  const row = page.getByTestId('computer-880615746');
  await row.getByRole('button', { name: 'Rename Front desk 1' }).click();
  const input = row.getByLabel('New name for Front desk 1');
  await input.fill('Reception 1');
  await input.press('Enter');
  await expect(row).toContainText('Reception 1');
  await expect(page.getByRole('status').filter({ hasText: 'Renamed to Reception 1' })).toBeVisible();

  await row.getByRole('button', { name: 'Revoke Reception 1' }).click();
  const dlg = page.getByRole('dialog');
  await expect(dlg).toContainText('live sessions to it end now');
  await shot(page, '06-computers-revoke-confirm');
  await dlg.getByRole('button', { name: 'Revoke' }).click();
  await expect(row.getByText('Revoked', { exact: true })).toBeVisible();
  await expect(row.getByRole('button', { name: /Revoke/ })).toHaveCount(0);

  // Filters
  await page.getByLabel('Status').selectOption('revoked');
  await expect(page.locator('tbody tr')).toHaveCount(2);
  await page.getByRole('button', { name: 'Clear filters' }).click();
  await page.getByLabel('Search computers').fill('op-0');
  await expect(page.locator('tbody tr')).toHaveCount(4);
});

test('enrollment token shows its secret once', async ({ page }) => {
  await signIn(page, 'dana@northwind.test');
  await page.getByRole('link', { name: 'Enrollment tokens' }).click();
  await expect(page.locator('tbody tr')).toHaveCount(2);
  await shot(page, '07-tokens');
  await page.getByRole('button', { name: 'New enrollment token' }).click();
  await page.getByLabel('Label').fill('Hygiene rooms');
  await page.getByLabel('Group').selectOption({ label: 'Operatories' });
  await page.getByLabel('Max uses').fill('4');
  await page.getByRole('button', { name: 'Create token' }).click();
  await expect(page.getByTestId('token-secret')).toHaveText(/^rcet_/);
  await shot(page, '08-token-created');
  await page.getByRole('button', { name: 'Done' }).click();
  await expect(page.locator('tbody tr')).toHaveCount(3);
  await expect(page.getByText('rcet_')).toHaveCount(0);
});

test('invite a person', async ({ page }) => {
  await signIn(page, 'dana@northwind.test');
  await tab(page, 'People').click();
  await expect(page.getByRole('heading', { name: /^People/ })).toBeVisible();
  await page.getByLabel('Email').fill('new.hire@northwind.test');
  await page.getByLabel('Role', { exact: true }).selectOption('viewer');
  await page.getByRole('button', { name: 'Send invite' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Invite sent to new.hire@northwind.test' })).toBeVisible();
  const invites = page.getByRole('region', { name: /Pending invites/ });
  await expect(invites.getByRole('cell', { name: 'new.hire@northwind.test', exact: true })).toBeVisible();
  await shot(page, '09-people');

  // Accept it as the new person, through the magic link.
  const invite = await page.evaluate(() => window.__rconnOutbox!.at(-1)!.link);
  await signOut(page);
  await page.goto(invite.replace(/^\/admin\//, ''));
  await expect(page.getByRole('heading', { name: 'You’re invited' })).toBeVisible();
  await shot(page, '10-invite-signed-out');
  await page.getByRole('link', { name: 'Sign in to accept' }).click();
  await page.getByLabel('Work email').fill('new.hire@northwind.test');
  await page.getByRole('button', { name: 'Continue' }).click();
  await page.getByRole('button', { name: 'Email me a sign-in link' }).click();
  await expect(page.getByRole('heading', { name: 'Check your email' })).toBeVisible();
  const magic = await page.evaluate(() => window.__rconnOutbox!.at(-1)!.link);
  await page.goto(magic.replace(/^\/admin\//, ''));
  await page.getByRole('button', { name: 'Sign in' }).click();
  // Same browser: the magic link returns to the invite.
  await expect(page.getByRole('heading', { name: 'Join the company' })).toBeVisible();
  await shot(page, '11-invite-accept');
  await page.getByRole('button', { name: 'Accept invite' }).click();
  await expect(page.getByRole('heading', { name: /^Computers/ })).toBeVisible();
  await expect(page.getByText('Viewer', { exact: true }).first()).toBeVisible();
});

test('admin cannot change an owner', async ({ page }) => {
  await signIn(page, 'sam@northwind.test');
  await tab(page, 'People').click();
  const dana = page.getByRole('row', { name: /Dana Reyes/ });
  await expect(dana.getByRole('combobox')).toHaveCount(0);
  await expect(dana.getByRole('button', { name: /Remove/ })).toHaveCount(0);
  await expect(page.getByRole('row', { name: /Leo Martins/ }).getByRole('combobox')).toBeVisible();
  await expect(page.getByLabel('Role', { exact: true }).locator('option')).toHaveText(['Admin', 'Technician', 'Viewer']);
});

test('edit a policy', async ({ page }) => {
  await signIn(page, 'dana@northwind.test');
  await tab(page, 'Groups & policies').click();
  await page.getByRole('button', { name: /^Operatories/ }).click();
  await expect(page.getByRole('heading', { name: 'Operatories' })).toBeVisible();
  await expect(page.getByLabel('Time zone')).toHaveValue('America/Chicago');
  const clip = page.getByRole('switch', { name: 'Clipboard' });
  await expect(clip).toBeChecked();
  await clip.uncheck();
  await page.getByRole('group', { name: 'Days' }).getByText('Sat').click();
  await page.getByLabel('Until').fill('19:30');
  await expect(page.getByText('Unsaved changes')).toBeVisible();
  await shot(page, '12-groups-policy');
  await page.getByRole('button', { name: 'Save policy' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Policy saved' })).toBeVisible();
  await expect(page.getByText('Unsaved changes')).toHaveCount(0);
  // Survives a reload (mock state is kept per tab).
  await page.reload();
  await expect(page.getByRole('switch', { name: 'Clipboard' })).not.toBeChecked();
  await expect(page.getByLabel('Until')).toHaveValue('19:30');

  // Groups: create and rename.
  await page.getByLabel('New group name').fill('Lab');
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Lab' })).toBeVisible();
});

test('export audit CSV', async ({ page }) => {
  await signIn(page, 'dana@northwind.test');
  await tab(page, 'Audit').click();
  await page.getByLabel('Time range').selectOption('all');
  await expect(page.locator('tbody tr')).toHaveCount(50);
  await page.getByRole('button', { name: 'Load more' }).click();
  await expect(page.locator('tbody tr')).toHaveCount(100);
  await page.getByLabel('Event type').selectOption('session.start');
  await expect(page.locator('tbody tr').first()).toContainText('Session started');
  await page.getByLabel('Event type').selectOption('');
  await expect(page.locator('tbody:not(.skeleton) tr')).toHaveCount(50);
  await shot(page, '13-audit');
  const [download] = await Promise.all([page.waitForEvent('download'), page.getByRole('link', { name: 'Export CSV' }).click()]);
  expect(download.suggestedFilename()).toBe('rconn-audit.csv');
  const csv = readFileSync(await download.path(), 'utf8');
  expect(csv.split('\n')[0]).toBe('at,kind,actor_type,actor,target_type,target,ip,detail');
  expect(csv.trim().split('\n').length).toBeGreaterThan(150);
});

test('settings, SSO and passkeys', async ({ page }) => {
  await signIn(page, 'dana@northwind.test');
  await tab(page, 'Settings').click();
  await expect(page.getByLabel('Name')).toHaveValue('Northwind Dental');
  await page.getByLabel('Domain').fill('northwind.test');
  await page.getByRole('button', { name: 'Add domain' }).click();
  await expect(page.getByText('Not verified')).toBeVisible();
  await shot(page, '14-settings-sso');
  await page.getByRole('button', { name: 'Verify' }).click();
  await expect(page.getByText('Verified', { exact: true })).toBeVisible();

  await page.getByRole('combobox', { name: 'Company' }).selectOption({ label: 'Harbor Accounting' });
  await expect(page.getByText('Only owners can see and change single sign-on.')).toBeVisible();

  await page.getByRole('button', { name: /Account menu/ }).click();
  await page.getByRole('menuitem', { name: 'Account and passkeys' }).click();
  await expect(page.getByRole('cell', { name: 'MacBook Touch ID', exact: true })).toBeVisible();
  await shot(page, '15-account-passkeys');
});

test('viewer sees no admin controls', async ({ page }) => {
  await signIn(page, 'leo@northwind.test');
  await expect(page.locator('tbody tr')).toHaveCount(18);
  await expect(page.getByRole('button', { name: /^Revoke/ })).toHaveCount(0);
  await expect(page.getByRole('button', { name: /^Rename/ })).toHaveCount(0);
  await expect(page.getByRole('link', { name: 'Enrollment tokens' })).toHaveCount(0);
  await expect(tab(page, 'Audit')).toHaveCount(0);
  await shot(page, '16-computers-viewer');
  await tab(page, 'Groups & policies').click();
  await expect(page.getByRole('switch', { name: 'Clipboard' })).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Save policy' })).toHaveCount(0);
  await tab(page, 'People').click();
  await expect(page.getByRole('button', { name: 'Send invite' })).toHaveCount(0);
  // Another company's URL looks like a missing one.
  await page.goto('o/b2e4f6a8-1c3d-4e5f-8a7b-9c0d1e2f3a4b/computers');
  await expect(page.getByRole('heading', { name: 'Company not found' })).toBeVisible();
});

for (const width of [1024, 390]) {
  test(`no horizontal page scroll at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 860 });
    await signIn(page, 'dana@northwind.test');
    for (const name of ['Computers', 'People', 'Groups & policies', 'Audit', 'Settings']) {
      await tab(page, name).click();
      await page.waitForLoadState('networkidle');
      await page.waitForTimeout(300);
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
      expect(overflow, `${name} at ${width}px`).toBeLessThanOrEqual(0);
      const slug = name.toLowerCase().replace(/[^a-z]+/g, '-');
      await shot(page, `w${width}-${slug}`);
    }
  });
}
