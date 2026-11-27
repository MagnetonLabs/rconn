import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError, api } from '../src/api/http';
import * as auth from '../src/api/auth';
import * as computers from '../src/api/computers';
import * as people from '../src/api/people';
import * as policies from '../src/api/policies';
import * as audit from '../src/api/audit';
import * as sso from '../src/api/sso';
import { createMockServer, type MockServer } from '../src/mock/server';
import { installMockFetch, memoryStore } from '../src/mock/install';
import { DEMO_PASSWORD, ORG_A, ORG_B } from '../src/mock/seed';

let server: MockServer;
let uninstall: () => void;

beforeEach(() => {
  server = createMockServer();
  uninstall = installMockFetch(server, memoryStore());
});
afterEach(() => uninstall());

const signIn = (email: string) => auth.signInPassword(email, DEMO_PASSWORD);

async function expectError(p: Promise<unknown>, status: number, code?: string) {
  const e = await p.then(
    () => null,
    (x) => x,
  );
  expect(e).toBeInstanceOf(ApiError);
  expect((e as ApiError).status).toBe(status);
  if (code) expect((e as ApiError).code).toBe(code);
}

describe('transport', () => {
  it('sends JSON with a body on every non-GET and parses the error envelope', async () => {
    const spy = vi.fn(async () => new Response(JSON.stringify({ error: { code: 'conflict', message: 'Nope' } }), { status: 409 }));
    const real = globalThis.fetch;
    globalThis.fetch = spy as unknown as typeof fetch;
    try {
      await expectError(api('POST', '/x'), 409, 'conflict');
      const [url, init] = spy.mock.calls[0] as unknown as [string, RequestInit];
      expect(url).toBe('/api/v1/x');
      expect((init.headers as Record<string, string>)['Content-Type']).toBe('application/json');
      expect(init.body).toBe('{}');
    } finally {
      globalThis.fetch = real;
    }
  });

  it('drops empty query values', async () => {
    const spy = vi.fn(async () => new Response('[]', { status: 200 }));
    const real = globalThis.fetch;
    globalThis.fetch = spy as unknown as typeof fetch;
    try {
      await computers.listComputers('o1', { q: '', status: 'online' });
      expect((spy.mock.calls[0] as unknown as [string])[0]).toBe('/api/v1/orgs/o1/computers?status=online');
    } finally {
      globalThis.fetch = real;
    }
  });
});

describe('auth', () => {
  it('gives the same 401 for unknown email and wrong password', async () => {
    await expectError(auth.signInPassword('nobody@x.test', 'x'), 401, 'unauthorized');
    await expectError(auth.signInPassword('dana@northwind.test', 'wrong'), 401, 'unauthorized');
  });

  it('signs in and lists memberships', async () => {
    const me = await signIn('dana@northwind.test');
    expect(me.user.name).toBe('Dana Reyes');
    expect(me.memberships.map((m) => m.role).sort()).toEqual(['admin', 'owner']);
    expect((await auth.getMe()).user.email).toBe('dana@northwind.test');
    await auth.logout();
    await expectError(auth.getMe(), 401);
  });

  it('offers SSO only for verified domains', async () => {
    expect((await auth.getOptions('grace@harbor.test')).sso?.org_id).toBe(ORG_B);
    const unknown = await auth.getOptions('who@nowhere.test');
    expect(unknown).toEqual({ password: true, magic_link: true, sso: null, passkey: true });
  });

  it('asks for the code when a magic link is opened in another browser', async () => {
    await expectError(auth.redeemMagic('other-browser'), 400, 'needs_code');
    await expectError(auth.redeemMagic('other-browser', '000000'), 400, 'invalid');
    const me = await auth.redeemMagic('other-browser', '246810');
    expect(me.user.email).toBe('dana@northwind.test');
    await expectError(auth.redeemMagic('other-browser', '246810'), 400, 'invalid');
  });

  it('sends a magic link that signs in once', async () => {
    await auth.requestMagicLink('sam@northwind.test');
    const mail = server.db.outbox.at(-1)!;
    const token = new URL(mail.link, 'http://x').searchParams.get('token')!;
    expect((await auth.redeemMagic(token)).user.email).toBe('sam@northwind.test');
    await expectError(auth.redeemMagic(token), 400, 'invalid');
  });
});

describe('tenancy', () => {
  it('answers 404, never 403, for another company', async () => {
    await signIn('sam@northwind.test'); // admin of Northwind only
    await expectError(computers.listComputers(ORG_B), 404, 'not_found');
    await expectError(audit.listAudit(ORG_B, {}), 404);
    const bComputer = server.db.computers.find((c) => c.org_id === ORG_B)!;
    await expectError(computers.updateComputer(ORG_A, bComputer.id, { name: 'x' }), 404);
    await expectError(computers.revokeComputer(ORG_A, bComputer.id), 404);
    const bGroup = server.db.groups.find((g) => g.org_id === ORG_B)!;
    const aComputer = server.db.computers.find((c) => c.org_id === ORG_A && !c.revoked_at)!;
    await expectError(computers.updateComputer(ORG_A, aComputer.id, { group_id: bGroup.id }), 404);
    await expectError(policies.getPolicy(ORG_A, bGroup.id), 404);
  });
});

describe('roles', () => {
  it('lets a viewer read computers but not change them or see audit', async () => {
    await signIn('leo@northwind.test');
    const list = await computers.listComputers(ORG_A);
    expect(list.length).toBe(17);
    await expectError(computers.updateComputer(ORG_A, list[0].id, { name: 'x' }), 403, 'forbidden');
    await expectError(computers.listTokens(ORG_A), 403);
    await expectError(audit.listAudit(ORG_A, {}), 403);
    await expectError(people.sendInvite(ORG_A, 'a@b.test', 'viewer'), 403);
    await expectError(policies.putPolicy(ORG_A, null, policies.DEFAULT_POLICY), 403);
  });

  it('stops admins from touching owners and SSO', async () => {
    await signIn('sam@northwind.test');
    const dana = server.db.users.find((u) => u.email === 'dana@northwind.test')!;
    await expectError(people.changeRole(ORG_A, dana.id, 'viewer'), 403);
    await expectError(people.removePerson(ORG_A, dana.id), 403);
    await expectError(people.sendInvite(ORG_A, 'boss@northwind.test', 'owner'), 403);
    await expectError(sso.getSso(ORG_A), 403);
    const leo = server.db.users.find((u) => u.email === 'leo@northwind.test')!;
    expect((await people.changeRole(ORG_A, leo.id, 'technician')).role).toBe('technician');
  });

  it('keeps the last owner', async () => {
    const me = await signIn('dana@northwind.test');
    await expectError(people.changeRole(ORG_A, me.user.id, 'admin'), 409, 'conflict');
  });
});

describe('computers, tokens, policies, audit', () => {
  it('renames, filters and revokes', async () => {
    await signIn('sam@northwind.test');
    const byId = await computers.listComputers(ORG_A, { q: '880615' });
    expect(byId.map((c) => c.short_code)).toEqual(['880 615 746']);
    const renamed = await computers.updateComputer(ORG_A, byId[0].id, { name: 'Reception 1' });
    expect(renamed.name).toBe('Reception 1');
    const revoked = await computers.revokeComputer(ORG_A, byId[0].id);
    expect(revoked.revoked_at).not.toBeNull();
    expect(revoked.online).toBe(false);
    const rev = await computers.listComputers(ORG_A, { status: 'revoked' });
    expect(rev.some((c) => c.id === byId[0].id)).toBe(true);
    const first = (await audit.listAudit(ORG_A, {})).items[0];
    expect(first.kind).toBe('computer.revoked');
  });

  it('returns the token secret only on create', async () => {
    await signIn('sam@northwind.test');
    await expectError(computers.createToken(ORG_A, { label: 'x', expires_in_hours: 721 }), 400, 'invalid');
    const t = await computers.createToken(ORG_A, { label: 'Lab', expires_in_hours: 24, max_uses: 3 });
    expect(t.token).toMatch(/^rcet_/);
    const list = await computers.listTokens(ORG_A);
    expect(list.find((x) => x.id === t.id)).not.toHaveProperty('token');
    await computers.deleteToken(ORG_A, t.id);
    expect((await computers.listTokens(ORG_A)).some((x) => x.id === t.id)).toBe(false);
  });

  it('validates and stores a policy', async () => {
    await signIn('sam@northwind.test');
    const bad = { ...policies.DEFAULT_POLICY, allowed_hours: { timezone: 'Mars/Base', days: ['mon' as const], start: '08:00', end: '17:00' } };
    await expectError(policies.putPolicy(ORG_A, null, bad), 400, 'invalid');
    const good = { ...bad, allowed_hours: { ...bad.allowed_hours, timezone: 'America/Chicago' } };
    expect((await policies.putPolicy(ORG_A, null, good)).allowed_hours?.timezone).toBe('America/Chicago');
    expect((await policies.getPolicy(ORG_A, null)).allowed_hours?.days).toEqual(['mon']);
    const changed = (await audit.listAudit(ORG_A, { kind: 'policy.changed' })).items[0];
    expect(changed.detail.changed).toEqual(['allowed_hours']);
    expect((changed.detail.after as policies.Policy).allowed_hours?.timezone).toBe('America/Chicago');
  });

  it('pages audit events with a cursor and exports CSV', async () => {
    await signIn('dana@northwind.test');
    const p1 = await audit.listAudit(ORG_A, {}, null, 60);
    expect(p1.items).toHaveLength(60);
    const p2 = await audit.listAudit(ORG_A, {}, p1.next, 60);
    const p3 = await audit.listAudit(ORG_A, {}, p2.next, 60);
    expect(p3.next).toBeNull();
    expect(p1.items.length + p2.items.length + p3.items.length).toBeGreaterThanOrEqual(150);
    const sessions = await audit.listAudit(ORG_A, { kind: 'session.start' }, null, 200);
    expect(sessions.items.every((e) => e.kind === 'session.start')).toBe(true);
    const res = await fetch(audit.auditCsvUrl(ORG_A, { kind: 'session.start' }));
    const csv = await res.text();
    expect(res.headers.get('Content-Type')).toContain('text/csv');
    expect(csv.split('\n')[0]).toBe('at,kind,actor_type,actor,target_type,target,ip,detail');
    expect(csv.trim().split('\n')).toHaveLength(sessions.items.length + 1);
  });
});
