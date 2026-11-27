import { expect, test, type Browser, type BrowserContext, type Page } from '@playwright/test';
import { spawn, type ChildProcess } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

// The whole portal against a real rconn-signaling (scripts/portal-e2e-real.sh starts it
// with a fresh data dir and an owner account). Mail goes to the server log, which is where
// the magic links and invites are read from. Tests share state and run in order.

const env = (k: string) => {
  const v = process.env[k];
  if (!v) throw new Error(`${k} is not set: run scripts/portal-e2e-real.sh`);
  return v;
};
const BASE = env('RCONN_E2E_BASE');
const LOG = env('RCONN_E2E_LOG');
const WORK = env('RCONN_E2E_WORK');
const OWNER = env('RCONN_E2E_OWNER_EMAIL');
const OWNER_PASSWORD = env('RCONN_E2E_OWNER_PASSWORD');
const HOST_BIN = env('RCONN_E2E_HOST_BIN');
const WS = env('RCONN_E2E_WS');
const SERVER_ID = env('RCONN_E2E_SERVER_ID');
const SHOTS = process.env.PORTAL_SHOTS ?? join(WORK, 'shots');
mkdirSync(SHOTS, { recursive: true });

const INVITEE = 'new.hire@acme.test';
const OTHER_OWNER = 'owner@beta.test';

test.describe.configure({ mode: 'serial' });

const shot = (page: Page, name: string) => page.screenshot({ path: join(SHOTS, `${name}.png`), fullPage: true });
const tab = (page: Page, name: string) => page.getByRole('navigation', { name: 'Sections' }).getByRole('link', { name });
const toast = (page: Page, text: string | RegExp) => page.getByRole('status').filter({ hasText: text });

// -- mail from the server log --------------------------------------------------------

interface Mail {
  to: string;
  subject: string;
  text: string;
}

function mails(): Mail[] {
  // eslint-disable-next-line no-control-regex
  const log = readFileSync(LOG, 'utf8').replace(/\x1b\[[0-9;]*m/g, '');
  const out: Mail[] = [];
  for (const entry of log.split(/\n(?=\d{4}-\d\d-\d\dT)/)) {
    if (!entry.includes('message follows')) continue;
    const to = / to=(\S+)/.exec(entry)?.[1] ?? '';
    const subject = / subject=(.*)$/m.exec(entry)?.[1]?.trim() ?? '';
    out.push({ to, subject, text: entry });
  }
  return out;
}

/** The newest mail to `to` after `seen` earlier ones (waits for the background sender). */
async function nextMail(to: string, seen: number): Promise<Mail> {
  let found: Mail | undefined;
  await expect
    .poll(() => {
      const m = mails().filter((x) => x.to === to);
      found = m.length > seen ? m[m.length - 1] : undefined;
      return found !== undefined;
    }, { message: `mail to ${to}` })
    .toBe(true);
  return found!;
}
const mailCount = (to: string) => mails().filter((m) => m.to === to).length;

function linkIn(mail: Mail, path: 'magic' | 'invite'): string {
  const m = new RegExp(`${BASE.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}/admin/(${path}\\?token=[A-Za-z0-9_-]{43})`).exec(mail.text);
  if (!m) throw new Error(`no ${path} link in mail to ${mail.to}:\n${mail.text}`);
  return m[1];
}
const codeIn = (mail: Mail) => {
  const m = /enter this code: (\d{3}) (\d{3})/.exec(mail.text);
  if (!m) throw new Error(`no code in mail to ${mail.to}`);
  return m[1] + m[2];
};

// -- sign-in helpers -----------------------------------------------------------------

async function newPage(browser: Browser): Promise<[BrowserContext, Page]> {
  const ctx = await browser.newContext({ baseURL: `${BASE}/admin/` });
  return [ctx, await ctx.newPage()];
}

async function askMagicLink(page: Page, email: string) {
  await page.getByLabel('Work email').fill(email);
  await page.getByRole('button', { name: 'Continue' }).click();
  const before = mailCount(email);
  await page.getByRole('button', { name: 'Email me a sign-in link' }).click();
  await expect(page.getByRole('heading', { name: 'Check your email' })).toBeVisible();
  return nextMail(email, before);
}

async function signInByLink(page: Page, email: string) {
  await page.goto('signin');
  const mail = await askMagicLink(page, email);
  await page.goto(linkIn(mail, 'magic'));
  await page.getByRole('button', { name: 'Sign in' }).click();
}

// -- a host on the signaling WebSocket -------------------------------------------------

class Host {
  readonly lines: string[] = [];
  readonly proc: ChildProcess;
  constructor(tokenFile: string, dir: string, label: string) {
    this.proc = spawn(HOST_BIN, [WS, SERVER_ID, tokenFile, dir, label], { stdio: ['ignore', 'pipe', 'pipe'] });
    let buf = '';
    this.proc.stdout!.on('data', (d: Buffer) => {
      buf += d.toString();
      const parts = buf.split('\n');
      buf = parts.pop()!;
      this.lines.push(...parts);
    });
    this.proc.stderr!.on('data', (d: Buffer) => process.stderr.write(`[host] ${d}`));
  }
  async waitFor(prefix: string, timeout = 15_000) {
    await expect.poll(() => this.lines.some((l) => l.startsWith(prefix)), { timeout, message: `host: ${prefix}` }).toBe(true);
    return this.lines.find((l) => l.startsWith(prefix))!;
  }
  stop() {
    this.proc.kill();
  }
}

// -- shared state --------------------------------------------------------------------

let owner: Page;
let ownerCtx: BrowserContext;
let orgId = '';
let host: Host | undefined;

test.beforeAll(async ({ browser }) => {
  [ownerCtx, owner] = await newPage(browser);
});
test.afterAll(async () => {
  host?.stop();
  await ownerCtx?.close();
});

// -- sign-in -------------------------------------------------------------------------

test('password sign-in sets a __Host- session cookie on http://localhost', async () => {
  const page = owner;
  await page.goto('');
  await expect(page).toHaveURL(/\/admin\/signin$/);
  await expect(page.getByRole('heading', { name: 'Sign in to the admin portal' })).toBeVisible();
  await shot(page, '01-signin');
  await page.getByLabel('Work email').fill(OWNER);
  await page.getByRole('button', { name: 'Continue' }).click();
  await expect(page.getByRole('button', { name: 'Email me a sign-in link' })).toBeVisible();
  // No passkeys on an http portal URL: no passkey button.
  await expect(page.getByRole('button', { name: /passkey/i })).toHaveCount(0);
  await page.getByLabel('Password').fill('not-the-password');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('alert')).toContainText('don’t match');
  await shot(page, '02-signin-password');
  await page.getByLabel('Password').fill(OWNER_PASSWORD);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('heading', { name: /^Computers/ })).toBeVisible();
  orgId = /\/o\/([0-9a-f-]{36})\//.exec(page.url())![1];
  const cookies = await ownerCtx.cookies();
  const session = cookies.find((c) => c.name === '__Host-rconn_session');
  expect(session, 'session cookie stored').toBeTruthy();
  expect(session!.secure && session!.httpOnly && session!.sameSite === 'Lax').toBe(true);
  await expect(page.getByText('No computers yet')).toBeVisible();
  await shot(page, '03-computers-empty');
});

test('magic link in the same browser, and the 6-digit code in another', async ({ browser }) => {
  const [ctxA, a] = await newPage(browser);
  const [ctxB, b] = await newPage(browser);
  const [ctxC, c] = await newPage(browser);
  try {
    // Same browser: the nonce cookie matches, no code needed.
    await a.goto('signin');
    const first = await askMagicLink(a, OWNER);
    await shot(a, '04-magic-sent');
    await a.goto(linkIn(first, 'magic'));
    await expect(a.getByRole('heading', { name: 'Sign in to Rconn' })).toBeVisible();
    await shot(a, '05-magic-landing');
    await a.getByRole('button', { name: 'Sign in' }).click();
    await expect(a.getByRole('heading', { name: /^Computers/ })).toBeVisible();
    // Single use.
    await a.goto(linkIn(first, 'magic'));
    await a.getByRole('button', { name: 'Sign in' }).click();
    await expect(a.getByRole('heading', { name: 'This link has expired' })).toBeVisible();

    // Asked for in C (C's nonce cookie), opened in B: B needs the code from the email.
    await c.goto('signin');
    const second = await askMagicLink(c, OWNER);
    await b.goto(linkIn(second, 'magic'));
    await b.getByRole('button', { name: 'Sign in' }).click();
    await expect(b.getByRole('heading', { name: 'Enter the code from the email' })).toBeVisible();
    const code = codeIn(second);
    const wrong = code === '000000' ? '111111' : '000000';
    await b.getByLabel('6-digit code').fill(wrong);
    await b.getByRole('button', { name: 'Sign in' }).click();
    await expect(b.getByRole('alert')).toContainText(/wrong code/i);
    await b.getByLabel('6-digit code').fill(code);
    await shot(b, '06-magic-code');
    await b.getByRole('button', { name: 'Sign in' }).click();
    await expect(b.getByRole('heading', { name: /^Computers/ })).toBeVisible();
  } finally {
    await ctxA.close();
    await ctxB.close();
    await ctxC.close();
  }
});

// -- computers -----------------------------------------------------------------------

test('enroll a real host with a token, rename, move to a group, revoke', async () => {
  const page = owner;
  // A group to move it into.
  await tab(page, 'Groups & policies').click();
  await page.getByLabel('New group name').fill('Front desk');
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Front desk' })).toBeVisible();

  await tab(page, 'Computers').click();
  await page.getByRole('link', { name: 'Enrollment tokens' }).click();
  await page.getByRole('button', { name: 'New enrollment token' }).click();
  await page.getByLabel('Label').fill('E2E rollout');
  await page.getByLabel('Max uses').fill('1');
  await page.getByRole('button', { name: 'Create token' }).click();
  const secret = (await page.getByTestId('token-secret').textContent())!.trim();
  expect(secret).toMatch(/^rcet_/);
  await shot(page, '07-token-created');
  await page.getByRole('button', { name: 'Done' }).click();
  await expect(page.locator('tbody tr')).toHaveCount(1);
  await expect(page.getByText(secret)).toHaveCount(0);
  await shot(page, '08-tokens');

  // Watch the list while the computer joins: it shows up without a reload (the list polls).
  await tab(page, 'Computers').click();
  await expect(page.getByText('No computers yet')).toBeVisible();
  const tokenFile = join(WORK, 'enroll.token');
  writeFileSync(tokenFile, secret + '\n', { mode: 0o600 });
  host = new Host(tokenFile, join(WORK, 'host-identity'), 'e2e-host');
  const enrolled = await host.waitFor('enrolled ');
  const hostId = enrolled.split(' ')[1];
  await host.waitFor('online');

  const row = page.locator('tbody tr').filter({ hasText: 'e2e-host' });
  await expect(row).toHaveCount(1, { timeout: 15_000 });
  await expect(row.getByRole('img', { name: 'Online' })).toBeVisible();
  await expect(page.getByText('1 online')).toBeVisible();
  await shot(page, '09-computers-online');

  // The token was single-use.
  await page.getByRole('link', { name: 'Enrollment tokens' }).click();
  await page.reload();
  await expect(page.locator('tbody tr').first()).toContainText('1 of 1');
  await expect(page.locator('tbody tr').first()).toContainText('Used up');
  await page.getByRole('link', { name: 'Computers', exact: true }).first().click();

  await row.getByRole('button', { name: 'Rename e2e-host' }).click();
  const input = row.getByLabel('New name for e2e-host');
  await input.fill('Reception PC');
  await input.press('Enter');
  const renamed = page.locator('tbody tr').filter({ hasText: 'Reception PC' });
  await expect(renamed).toHaveCount(1);
  await expect(toast(page, 'Renamed to Reception PC')).toBeVisible();
  await expect(renamed).toContainText('e2e-host'); // hostname under the name

  await renamed.getByLabel('Group for Reception PC').selectOption({ label: 'Front desk' });
  await expect(toast(page, 'Reception PC moved to Front desk')).toBeVisible();
  await page.reload();
  await expect(page.locator('tbody tr').filter({ hasText: 'Reception PC' }).getByLabel('Group for Reception PC')).toHaveValue(/[0-9a-f-]{36}/);
  await page.getByLabel('Group', { exact: true }).selectOption({ label: 'Front desk' });
  await expect(page.locator('tbody tr')).toHaveCount(1);
  await page.getByRole('button', { name: 'Clear filters' }).click();

  const r2 = page.locator('tbody tr').filter({ hasText: 'Reception PC' });
  await r2.getByRole('button', { name: 'Revoke Reception PC' }).click();
  const dlg = page.getByRole('dialog');
  await expect(dlg).toContainText('live sessions to it end now');
  await shot(page, '10-computers-revoke');
  await dlg.getByRole('button', { name: 'Revoke' }).click();
  await expect(r2.getByText('Revoked', { exact: true })).toBeVisible();
  // The host is dropped and can't come back.
  await host.waitFor('evicted');
  await host.waitFor('refused');
  await page.reload();
  await expect(page.locator('tbody tr').filter({ hasText: 'Reception PC' }).getByText('Revoked', { exact: true })).toBeVisible();
  await page.getByLabel('Status').selectOption('revoked');
  await expect(page.locator('tbody tr')).toHaveCount(1);
  await shot(page, '11-computers-revoked');
  await page.getByRole('button', { name: 'Clear filters' }).click();
  expect(hostId).toMatch(/^[0-9a-f-]{36}$/);
});

// -- people --------------------------------------------------------------------------

test('invite, accept in another browser, change role, remove', async ({ browser }) => {
  const page = owner;
  await tab(page, 'People').click();
  await expect(page.getByRole('heading', { name: /^People/ })).toBeVisible();
  const before = mailCount(INVITEE);
  await page.getByLabel('Email').fill(INVITEE);
  await page.getByLabel('Role', { exact: true }).selectOption('technician');
  await page.getByRole('button', { name: 'Send invite' }).click();
  await expect(toast(page, `Invite sent to ${INVITEE}`)).toBeVisible();
  const invites = page.getByRole('region', { name: /Pending invites/ });
  await expect(invites.getByRole('cell', { name: INVITEE, exact: true })).toBeVisible();
  await shot(page, '12-people-invited');
  const invite = await nextMail(INVITEE, before);
  expect(invite.subject).toContain('Join ');

  const [ctx, p] = await newPage(browser);
  try {
    await p.goto(linkIn(invite, 'invite'));
    await expect(p.getByRole('heading', { name: 'You’re invited' })).toBeVisible();
    await shot(p, '13-invite-signed-out');
    await p.getByRole('link', { name: 'Sign in to accept' }).click();
    const magic = await askMagicLink(p, INVITEE);
    await p.goto(linkIn(magic, 'magic'));
    await p.getByRole('button', { name: 'Sign in' }).click();
    await expect(p.getByRole('heading', { name: 'Join the company' })).toBeVisible();
    await shot(p, '14-invite-accept');
    await p.getByRole('button', { name: 'Accept invite' }).click();
    await expect(p.getByRole('heading', { name: /^Computers/ })).toBeVisible();
    // A technician sees computers but no admin controls.
    await expect(p.getByRole('link', { name: 'Enrollment tokens' })).toHaveCount(0);
    await expect(tab(p, 'Audit')).toHaveCount(0);

    await page.reload();
    const person = page.getByRole('row', { name: new RegExp(INVITEE.replace('.', '\\.')) });
    await expect(person).toBeVisible();
    await expect(invites.getByRole('cell', { name: INVITEE, exact: true })).toHaveCount(0);
    await person.getByRole('combobox').selectOption('viewer');
    await expect(toast(page, /viewer/i)).toBeVisible();
    await page.reload();
    await expect(page.getByRole('row', { name: new RegExp(INVITEE.replace('.', '\\.')) }).getByRole('combobox')).toHaveValue('viewer');
    await shot(page, '15-people-role');

    await page.getByRole('row', { name: new RegExp(INVITEE.replace('.', '\\.')) }).getByRole('button', { name: /^Remove/ }).click();
    await page.getByRole('dialog').getByRole('button', { name: /Remove/ }).click();
    await expect(page.getByRole('row', { name: new RegExp(INVITEE.replace('.', '\\.')) })).toHaveCount(0);

    // Removing someone ends their sign-ins.
    await p.reload();
    await expect(p.getByRole('heading', { name: 'Sign in to the admin portal' })).toBeVisible();
    expect(await p.evaluate(async () => (await fetch('/api/v1/me')).status)).toBe(401);
  } finally {
    await ctx.close();
  }
});

// -- groups and policies -------------------------------------------------------------

test('group policy with allowed hours, and the company default', async () => {
  const page = owner;
  await tab(page, 'Groups & policies').click();
  await page.getByRole('button', { name: /^Front desk/ }).click();
  await expect(page.getByRole('heading', { name: 'Front desk' })).toBeVisible();
  const clip = page.getByRole('switch', { name: 'Clipboard' });
  await expect(clip).toBeChecked();
  await clip.uncheck();
  await page.getByRole('switch', { name: 'Only during set hours' }).check();
  await page.getByRole('group', { name: 'Days' }).getByText('Sat').click();
  await page.getByLabel('From').fill('07:30');
  await page.getByLabel('Until').fill('19:00');
  await page.getByLabel('Time zone').selectOption('America/Chicago');
  await expect(page.getByText('Unsaved changes')).toBeVisible();
  await shot(page, '16-group-policy');
  await page.getByRole('button', { name: 'Save policy' }).click();
  await expect(toast(page, 'Policy saved')).toBeVisible();
  await page.reload();
  await expect(page.getByRole('switch', { name: 'Clipboard' })).not.toBeChecked();
  await expect(page.getByLabel('From')).toHaveValue('07:30');
  await expect(page.getByLabel('Until')).toHaveValue('19:00');
  await expect(page.getByLabel('Time zone')).toHaveValue('America/Chicago');
  await expect(page.getByRole('group', { name: 'Days' }).getByRole('checkbox', { name: 'Sat' })).toBeChecked();
  await expect(page.getByRole('group', { name: 'Days' }).getByRole('checkbox', { name: 'Sun' })).not.toBeChecked();

  // Crossing midnight is allowed and explained.
  await page.getByLabel('From').fill('22:00');
  await page.getByLabel('Until').fill('06:00');
  await expect(page.getByText('Ends the next day')).toBeVisible();
  await page.getByRole('button', { name: 'Discard changes' }).click();

  await page.getByRole('button', { name: /^Company default/ }).click();
  await expect(page.getByRole('heading', { name: 'Company default' })).toBeVisible();
  const files = page.getByRole('switch', { name: 'File transfer' });
  await expect(files).toBeChecked();
  await files.uncheck();
  await page.getByRole('button', { name: 'Save policy' }).click();
  await expect(toast(page, 'Policy saved')).toBeVisible();
  await page.reload();
  await expect(page.getByRole('switch', { name: 'File transfer' })).not.toBeChecked();
  await expect(page.getByRole('switch', { name: 'Clipboard' })).toBeChecked();
  await shot(page, '17-default-policy');
});

// -- settings ------------------------------------------------------------------------

test('rename the company; SSO and passkey pages load', async () => {
  const page = owner;
  await tab(page, 'Settings').click();
  const name = page.getByLabel('Name', { exact: true });
  await expect(name).not.toHaveValue('');
  await name.fill('Acme Dental');
  await page.getByRole('button', { name: /^Save/ }).first().click();
  await expect(toast(page, /saved|renamed/i)).toBeVisible();
  await page.reload();
  await expect(page.getByLabel('Name', { exact: true })).toHaveValue('Acme Dental');
  // The company switcher in the header follows.
  await expect(page.getByRole('combobox', { name: 'Company' }).locator('option:checked')).toHaveText('Acme Dental');
  await expect(page.getByRole('heading', { name: /single sign-on/i })).toBeVisible();
  await expect(page.getByLabel('Issuer URL')).toBeVisible();
  await page.getByLabel('Domain').fill('acme.test');
  await page.getByRole('button', { name: 'Add domain' }).click();
  await expect(page.getByText('Not verified')).toBeVisible();
  await shot(page, '18-settings');

  await page.getByRole('button', { name: /Account menu/ }).click();
  await page.getByRole('menuitem', { name: 'Account and passkeys' }).click();
  await expect(page.getByRole('heading', { name: 'Passkeys', exact: true })).toBeVisible();
  await expect(page.getByText('No passkeys yet')).toBeVisible();
  // The server has passkeys off (http portal URL): no add button, an explanation instead.
  await expect(page.getByRole('button', { name: 'Add passkey' })).toHaveCount(0);
  await expect(page.getByText(/Passkeys need a secure \(https\) connection/)).toBeVisible();
  await shot(page, '19-account-passkeys');
});

// -- audit ---------------------------------------------------------------------------

test('audit shows what happened, filters by kind, exports CSV', async () => {
  const page = owner;
  await tab(page, 'Audit').click();
  await page.getByLabel('Time range').selectOption('all');
  const rows = page.locator('tbody:not(.skeleton) tr');
  await expect(rows.first()).toBeVisible();
  for (const label of [
    'Signed in',
    'Sign-in failed',
    'Token created',
    'Computer enrolled',
    'Computer renamed',
    'Computer revoked',
    'Person invited',
    'Person joined',
    'Role changed',
    'Person removed',
    'Policy changed',
    'Company renamed',
    'Group created',
  ]) {
    await expect(page.locator('tbody .badge').filter({ hasText: label }).first(), label).toBeVisible();
  }
  await shot(page, '20-audit');
  await page.getByLabel('Event type').selectOption('computer.revoked');
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText('Reception PC');
  await page.getByLabel('Event type').selectOption('computer.moved');
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText('No group → Front desk');
  await shot(page, '21-audit-filtered');
  await page.getByLabel('Event type').selectOption('');

  const [download] = await Promise.all([page.waitForEvent('download'), page.getByRole('link', { name: 'Export CSV' }).click()]);
  expect(download.suggestedFilename()).toBe('rconn-audit.csv');
  const csv = readFileSync(await download.path(), 'utf8');
  const lines = csv.trim().split('\n');
  expect(lines[0]).toBe('at,kind,actor_type,actor,target_type,target,ip,detail');
  const kinds = new Set(lines.slice(1).map((l) => l.split(',')[1]));
  for (const k of ['sign_in', 'sign_in.failed', 'token.created', 'computer.enrolled', 'computer.renamed', 'computer.moved', 'computer.revoked', 'person.invited', 'person.joined', 'person.role_changed', 'person.removed', 'policy.changed', 'org.renamed']) {
    expect(kinds, k).toContain(k);
  }
  expect(csv).toContain('Reception PC');
  expect(csv).toContain(INVITEE);
});

// -- another company -----------------------------------------------------------------

test('a second company sees nothing of the first', async ({ browser }) => {
  const [ctx, p] = await newPage(browser);
  try {
    await signInByLink(p, OTHER_OWNER);
    await expect(p.getByRole('heading', { name: 'You’re not in a company yet' })).toBeVisible();
    await p.getByLabel('Company name').fill('Beta Clinic');
    await p.getByRole('button', { name: 'Create company' }).click();
    await expect(p.getByRole('heading', { name: /^Computers/ })).toBeVisible();
    await expect(p.getByText('No computers yet')).toBeVisible();
    await shot(p, '22-other-company');

    for (const area of ['computers', 'people', 'groups', 'audit', 'settings']) {
      await p.goto(`o/${orgId}/${area}`);
      await expect(p.getByRole('heading', { name: 'Company not found' }), area).toBeVisible();
    }
    await shot(p, '23-other-company-404');
    const statuses = await p.evaluate(async (id) => {
      const paths = ['', '/computers', '/people', '/invites', '/groups', '/policy/default', '/audit', '/audit.csv', '/enrollment-tokens', '/sso'];
      const out: Record<string, number> = {};
      for (const path of paths) out[path] = (await fetch(`/api/v1/orgs/${id}${path}`)).status;
      out['PATCH'] = (
        await fetch(`/api/v1/orgs/${id}`, { method: 'PATCH', headers: { 'Content-Type': 'application/json' }, body: '{"name":"x"}' })
      ).status;
      return out;
    }, orgId);
    for (const [path, status] of Object.entries(statuses)) expect(status, path || '/').toBe(404);
  } finally {
    await ctx.close();
  }
});

test('sign out ends the session on the server', async () => {
  const page = owner;
  const before = (await ownerCtx.cookies()).find((c) => c.name === '__Host-rconn_session')!.value;
  await page.getByRole('button', { name: /Account menu/ }).click();
  await page.getByRole('menuitem', { name: 'Sign out' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in to the admin portal' })).toBeVisible();
  const status = await page.evaluate(async () => (await fetch('/api/v1/me')).status);
  expect(status).toBe(401);
  // The old cookie value is dead too, not just deleted from this browser.
  const replay = await fetch(`${BASE}/api/v1/me`, { headers: { cookie: `__Host-rconn_session=${before}` } });
  expect(replay.status).toBe(401);
});
