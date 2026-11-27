// In-memory implementation of the portal API v1 contract, used by `npm run dev:mock`, Vitest and
// Playwright. Enforces the role table and answers 404 for anything in another company.

import type { Me, Role } from '../api/auth';
import type { Computer } from '../api/computers';
import type { Policy, Day } from '../api/policies';
import { DAYS, DEFAULT_POLICY } from '../api/policies';
import type { AuditEvent } from '../api/audit';
import type { Db, DbAudit, DbComputer, DbMembership, DbUser } from './seed';
import { makeIds, seed } from './seed';

export interface MockRequest {
  method: string;
  /** Path after /api/v1, e.g. "/orgs/…/computers". */
  path: string;
  query: URLSearchParams;
  body: unknown;
  /** Session id from the mock cookie, if any. */
  session: string | null;
}

export interface MockResponse {
  status: number;
  json?: unknown;
  text?: string;
  contentType?: string;
  /** string = set the session cookie, null = clear it, undefined = leave it. */
  setSession?: string | null;
}

class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

const RANK: Record<Role, number> = { owner: 3, admin: 2, technician: 1, viewer: 0 };
const ROLES: Role[] = ['owner', 'admin', 'technician', 'viewer'];

const notFound = () => new HttpError(404, 'not_found', 'Not found.');
const forbidden = (m = 'Your role does not allow this.') => new HttpError(403, 'forbidden', m);
const invalid = (m: string) => new HttpError(400, 'invalid', m);
const conflict = (m: string) => new HttpError(409, 'conflict', m);

type Body = Record<string, unknown>;
type Ctx = { req: MockRequest; params: string[]; body: Body };
type Handler = (c: Ctx) => MockResponse;

export interface MockServer {
  db: Db;
  handle(req: MockRequest): MockResponse;
}

export function createMockServer(db: Db = seed()): MockServer {
  const ids = makeIds(db.seq + 7);
  const now = () => Date.now();
  const iso = (t = now()) => new Date(t).toISOString();

  const ok = (json: unknown, status = 200): MockResponse => ({ status, json });
  const empty = (status = 204): MockResponse => ({ status });

  function sessionUser(req: MockRequest): DbUser | null {
    if (!req.session) return null;
    const uid = db.sessions[req.session];
    return db.users.find((u) => u.id === uid) ?? null;
  }
  function requireUser(req: MockRequest): DbUser {
    const u = sessionUser(req);
    if (!u) throw new HttpError(401, 'unauthorized', 'Sign in first.');
    return u;
  }
  function meOf(u: DbUser): Me {
    return {
      user: { id: u.id, email: u.email, name: u.name },
      memberships: db.memberships
        .filter((m) => m.user_id === u.id)
        .map((m) => ({ org_id: m.org_id, org_name: db.orgs.find((o) => o.id === m.org_id)!.name, role: m.role })),
    };
  }
  function startSession(u: DbUser): MockResponse {
    const sid = ids.secret(24);
    db.sessions[sid] = u.id;
    const m = db.memberships.find((x) => x.user_id === u.id);
    if (m) audit(m.org_id, 'sign_in', u, null, { method: 'password' });
    return { status: 200, json: meOf(u), setSession: sid };
  }
  /** Membership of the caller in `orgId`, or 404 (never 403) when they are not a member. */
  function member(c: Ctx): { user: DbUser; m: DbMembership; orgId: string } {
    const user = requireUser(c.req);
    const orgId = c.params[0];
    const m = db.memberships.find((x) => x.org_id === orgId && x.user_id === user.id);
    if (!m) throw notFound();
    return { user, m, orgId };
  }
  function need(m: DbMembership, min: Role) {
    if (RANK[m.role] < RANK[min]) throw forbidden();
  }
  function audit(
    org_id: string,
    kind: string,
    u: DbUser,
    target: AuditEvent['target'],
    detail: Record<string, unknown> = {},
  ) {
    db.audit.unshift({
      id: ids.uuid(),
      org_id,
      at: iso(),
      kind,
      actor: { type: 'user', id: u.id, label: u.email },
      target,
      ip: '127.0.0.1',
      detail,
    });
  }
  const str = (v: unknown) => (typeof v === 'string' ? v.trim() : '');
  const isEmail = (s: string) => /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(s);
  const strip = <T extends { org_id: string }>(x: T): Omit<T, 'org_id'> => {
    const { org_id: _o, ...rest } = x;
    void _o;
    return rest;
  };
  const computerOut = (c: DbComputer): Computer => strip(c);
  const groupOf = (orgId: string, id: unknown) => {
    if (id === null || id === undefined || id === '') return null;
    const g = db.groups.find((x) => x.org_id === orgId && x.id === id);
    if (!g) throw notFound();
    return g;
  };

  function validPolicy(b: Body): Policy {
    const p: Policy = { ...DEFAULT_POLICY };
    for (const k of ['clipboard', 'file_transfer', 'audio', 'require_approval', 'show_indicator', 'allow_access_password'] as const) {
      if (typeof b[k] !== 'boolean') throw invalid(`${k} must be true or false.`);
      p[k] = b[k] as boolean;
    }
    const h = b.allowed_hours as Body | null | undefined;
    if (h === null || h === undefined) p.allowed_hours = null;
    else {
      const tz = str(h.timezone);
      try {
        new Intl.DateTimeFormat('en', { timeZone: tz });
      } catch {
        throw invalid('Unknown time zone.');
      }
      if (!tz) throw invalid('Pick a time zone.');
      const days = Array.isArray(h.days) ? (h.days as string[]) : [];
      if (!days.length || days.some((d) => !DAYS.includes(d as Day))) throw invalid('Pick at least one day.');
      const hm = /^([01]\d|2[0-3]):[0-5]\d$/;
      if (!hm.test(str(h.start)) || !hm.test(str(h.end))) throw invalid('Times must be HH:MM.');
      if (h.start === h.end) throw invalid('Start and end must differ.');
      p.allowed_hours = { timezone: tz, days: DAYS.filter((d) => days.includes(d)), start: str(h.start), end: str(h.end) };
    }
    return p;
  }

  function personOut(m: DbMembership) {
    const u = db.users.find((x) => x.id === m.user_id)!;
    return { user_id: u.id, email: u.email, name: u.name, role: m.role, joined_at: m.joined_at, last_active: m.last_active };
  }

  function filterAudit(orgId: string, q: URLSearchParams): DbAudit[] {
    const from = q.get('from');
    const to = q.get('to');
    const kind = q.get('kind');
    const text = (q.get('q') ?? '').toLowerCase();
    return db.audit.filter((e) => {
      if (e.org_id !== orgId) return false;
      if (from && e.at < from) return false;
      if (to && e.at > to) return false;
      if (kind && e.kind !== kind) return false;
      if (text) {
        const hay = [e.kind, e.actor.label, e.target?.label ?? '', e.ip ?? '', JSON.stringify(e.detail)].join(' ').toLowerCase();
        if (!hay.includes(text)) return false;
      }
      return true;
    });
  }

  const csvCell = (v: string) => (/[",\n]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v);

  const ORG = '/orgs/([^/]+)';
  const routes: [string, RegExp, Handler][] = [];
  const on = (method: string, pattern: string, h: Handler) => routes.push([method, new RegExp(`^${pattern}$`), h]);

  // ---------- Auth ----------
  on('GET', '/auth/options', ({ req }) => {
    const email = (req.query.get('email') ?? '').trim().toLowerCase();
    const domain = email.split('@')[1] ?? '';
    let sso: { org_id: string; name: string } | null = null;
    for (const [orgId, s] of Object.entries(db.sso)) {
      if (s.enabled && s.domains.some((d) => d.verified && d.domain === domain)) {
        sso = { org_id: orgId, name: db.orgs.find((o) => o.id === orgId)!.name };
      }
    }
    // Same answer for known and unknown addresses: only the domain decides SSO.
    return ok({ password: true, magic_link: true, sso, passkey: true });
  });

  on('POST', '/auth/password', ({ body }) => {
    const email = str(body.email).toLowerCase();
    const fails = db.failures[email] ?? 0;
    if (fails >= 5) throw new HttpError(429, 'rate_limited', 'Too many attempts. Wait a minute and try again.');
    const u = db.users.find((x) => x.email === email);
    if (!u || !u.password || u.password !== body.password) {
      db.failures[email] = fails + 1;
      throw new HttpError(401, 'unauthorized', 'That email and password don’t match.');
    }
    db.failures[email] = 0;
    return startSession(u);
  });

  on('POST', '/auth/magic-link', ({ body }) => {
    const email = str(body.email).toLowerCase();
    const known = db.users.some((u) => u.email === email) || db.invites.some((i) => i.email === email);
    if (known) {
      const token = ids.secret(32);
      const code = ids.digits(6);
      db.magic.push({ token, email, code, bound: true, used: false, expires_at: now() + 15 * 60_000 });
      db.outbox.push({ to: email, subject: 'Sign in to Rconn', link: `/admin/magic?token=${token}`, code });
    }
    return empty(202);
  });

  on('POST', '/auth/magic', ({ body }) => {
    const token = str(body.token);
    const code = str(body.code);
    const ml = db.magic.find((x) => x.token === token);
    if (!ml || ml.used || ml.expires_at < now()) throw invalid('This sign-in link has expired or was already used.');
    if (!ml.bound && !code) throw new HttpError(400, 'needs_code', 'Enter the 6-digit code from the email.');
    if (code && code !== ml.code) throw invalid('That code is not right.');
    ml.used = true;
    let u = db.users.find((x) => x.email === ml.email);
    if (!u) {
      u = { id: ids.uuid(), email: ml.email, name: ml.email.split('@')[0], password: null };
      db.users.push(u);
    }
    return startSession(u);
  });

  on('POST', '/auth/logout', ({ req }) => {
    if (req.session) delete db.sessions[req.session];
    return { status: 204, setSession: null };
  });

  // Passkeys (wave 2). The mock accepts any credential the browser produces.
  let pendingLogin: string | null = null;
  const b64 = (s: string) => btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  on('POST', '/auth/passkey/register/start', ({ req }) => {
    const u = requireUser(req);
    return ok({
      publicKey: {
        rp: { name: 'Rconn', id: typeof location !== 'undefined' ? location.hostname : 'localhost' },
        user: { id: b64(u.id), name: u.email, displayName: u.name },
        challenge: b64(ids.secret(32)),
        pubKeyCredParams: [
          { type: 'public-key', alg: -7 },
          { type: 'public-key', alg: -257 },
        ],
        timeout: 60000,
        authenticatorSelection: { residentKey: 'preferred', userVerification: 'preferred' },
        excludeCredentials: [],
      },
    });
  });
  on('POST', '/auth/passkey/register/finish', ({ req, body }) => {
    const u = requireUser(req);
    const pk = { id: ids.uuid(), user_id: u.id, name: str(body.name) || 'Passkey', created_at: iso(), last_used: null };
    db.passkeys.push(pk);
    const { user_id: _u, ...out } = pk;
    void _u;
    return ok(out);
  });
  on('POST', '/auth/passkey/login/start', ({ body }) => {
    pendingLogin = str(body.email).toLowerCase();
    return ok({ publicKey: { challenge: b64(ids.secret(32)), timeout: 60000, userVerification: 'preferred', allowCredentials: [] } });
  });
  on('POST', '/auth/passkey/login/finish', () => {
    const u = db.users.find((x) => x.email === pendingLogin);
    if (!u || !db.passkeys.some((p) => p.user_id === u.id)) throw new HttpError(401, 'unauthorized', 'That passkey isn’t registered.');
    return startSession(u);
  });
  on('GET', '/me/passkeys', ({ req }) => {
    const u = requireUser(req);
    return ok(db.passkeys.filter((p) => p.user_id === u.id).map(({ user_id: _u, ...p }) => (void _u, p)));
  });
  on('DELETE', '/me/passkeys/([^/]+)', ({ req, params }) => {
    const u = requireUser(req);
    const i = db.passkeys.findIndex((p) => p.id === params[0] && p.user_id === u.id);
    if (i < 0) throw notFound();
    db.passkeys.splice(i, 1);
    return empty();
  });

  // ---------- Me ----------
  on('GET', '/me', ({ req }) => ok(meOf(requireUser(req))));
  on('PATCH', '/me', ({ req, body }) => {
    const u = requireUser(req);
    const name = str(body.name);
    if (!name) throw invalid('Enter a name.');
    u.name = name;
    return ok(meOf(u));
  });

  on('POST', '/invites/accept', ({ req, body }) => {
    const u = requireUser(req);
    const inv = db.invites.find((i) => i.token === str(body.token));
    if (!inv || inv.expires_at < iso()) throw invalid('This invite has expired or was cancelled.');
    if (inv.email !== u.email) throw forbidden(`This invite was sent to ${inv.email}. Sign in with that address.`);
    if (!db.memberships.some((m) => m.org_id === inv.org_id && m.user_id === u.id)) {
      db.memberships.push({ org_id: inv.org_id, user_id: u.id, role: inv.role, joined_at: iso(), last_active: iso() });
    }
    db.invites = db.invites.filter((i) => i !== inv);
    return ok(meOf(u));
  });

  // ---------- Company ----------
  on('POST', '/orgs', ({ req, body }) => {
    const u = requireUser(req);
    const name = str(body.name);
    if (!name) throw invalid('Enter a company name.');
    const org = { id: ids.uuid(), name, created_at: iso(), plan: 'free' };
    db.orgs.push(org);
    db.memberships.push({ org_id: org.id, user_id: u.id, role: 'owner', joined_at: iso(), last_active: iso() });
    db.policies[`${org.id}:default`] = { ...DEFAULT_POLICY };
    db.sso[org.id] = { enabled: false, issuer: '', client_id: '', client_secret: null, auto_join_role: 'viewer', domains: [] };
    return ok(org, 201);
  });
  on('GET', ORG, (c) => {
    const { orgId } = member(c);
    return ok(db.orgs.find((o) => o.id === orgId));
  });
  on('PATCH', ORG, (c) => {
    const { orgId, m, user } = member(c);
    need(m, 'owner');
    const name = str(c.body.name);
    if (!name) throw invalid('Enter a company name.');
    const org = db.orgs.find((o) => o.id === orgId)!;
    org.name = name;
    audit(orgId, 'org.renamed', user, { type: 'org', id: orgId, label: name });
    return ok(org);
  });

  // ---------- Computers ----------
  on('GET', `${ORG}/computers`, (c) => {
    const { orgId } = member(c);
    const q = (c.req.query.get('q') ?? '').toLowerCase().trim();
    const qDigits = q.replace(/\s/g, '');
    const group = c.req.query.get('group_id');
    const status = c.req.query.get('status');
    const out = db.computers.filter((x) => {
      if (x.org_id !== orgId) return false;
      if (group && x.group_id !== group) return false;
      if (status === 'online' && !(x.online && !x.revoked_at)) return false;
      if (status === 'offline' && (x.online || x.revoked_at)) return false;
      if (status === 'revoked' && !x.revoked_at) return false;
      if (q) {
        const hit =
          x.name.toLowerCase().includes(q) ||
          x.hostname.toLowerCase().includes(q) ||
          (qDigits.length > 0 && /^\d+$/.test(qDigits) && x.short_code.replace(/\s/g, '').includes(qDigits));
        if (!hit) return false;
      }
      return true;
    });
    return ok(out.map(computerOut));
  });
  const findComputer = (orgId: string, id: string) => {
    const x = db.computers.find((y) => y.org_id === orgId && y.id === id);
    if (!x) throw notFound();
    return x;
  };
  on('PATCH', `${ORG}/computers/([^/]+)`, (c) => {
    const { orgId, m, user } = member(c);
    const x = findComputer(orgId, c.params[1]);
    need(m, 'admin');
    if (x.revoked_at) throw conflict('This computer was revoked.');
    if ('name' in c.body) {
      const name = str(c.body.name);
      if (!name) throw invalid('Enter a name.');
      if (name.length > 64) throw invalid('Names are at most 64 characters.');
      if (name !== x.name) audit(orgId, 'computer.renamed', user, { type: 'computer', id: x.id, label: name }, { from: x.name, to: name });
      x.name = name;
    }
    if ('group_id' in c.body) {
      const g = groupOf(orgId, c.body.group_id);
      x.group_id = g?.id ?? null;
    }
    return ok(computerOut(x));
  });
  on('POST', `${ORG}/computers/([^/]+)/revoke`, (c) => {
    const { orgId, m, user } = member(c);
    const x = findComputer(orgId, c.params[1]);
    need(m, 'admin');
    if (!x.revoked_at) {
      x.revoked_at = iso();
      x.online = false;
      audit(orgId, 'computer.revoked', user, { type: 'computer', id: x.id, label: x.name });
    }
    return ok(computerOut(x));
  });

  on('GET', `${ORG}/enrollment-tokens`, (c) => {
    const { orgId, m } = member(c);
    need(m, 'admin');
    return ok(db.tokens.filter((t) => t.org_id === orgId).map(strip));
  });
  on('POST', `${ORG}/enrollment-tokens`, (c) => {
    const { orgId, m, user } = member(c);
    need(m, 'admin');
    const label = str(c.body.label);
    if (!label) throw invalid('Enter a label.');
    const hours = Number(c.body.expires_in_hours);
    if (!Number.isInteger(hours) || hours < 1 || hours > 720) throw invalid('Expiry must be 1 to 720 hours.');
    const max = c.body.max_uses;
    if (max !== undefined && max !== null && (!Number.isInteger(max) || (max as number) < 1)) throw invalid('Max uses must be a whole number.');
    const g = groupOf(orgId, c.body.group_id);
    const t = {
      id: ids.uuid(),
      org_id: orgId,
      label,
      group_id: g?.id ?? null,
      expires_at: iso(now() + hours * 3_600_000),
      uses: 0,
      max_uses: (max as number | undefined) ?? null,
      created_by: user.email,
      created_at: iso(),
    };
    db.tokens.unshift(t);
    audit(orgId, 'token.created', user, { type: 'token', id: t.id, label });
    return ok({ ...strip(t), token: `rcet_${ids.secret(40)}` }, 201);
  });
  on('DELETE', `${ORG}/enrollment-tokens/([^/]+)`, (c) => {
    const { orgId, m, user } = member(c);
    const t = db.tokens.find((x) => x.org_id === orgId && x.id === c.params[1]);
    if (!t) throw notFound();
    need(m, 'admin');
    db.tokens = db.tokens.filter((x) => x !== t);
    audit(orgId, 'token.deleted', user, { type: 'token', id: t.id, label: t.label });
    return empty();
  });

  // ---------- People ----------
  on('GET', `${ORG}/people`, (c) => {
    const { orgId } = member(c);
    return ok(db.memberships.filter((x) => x.org_id === orgId).map(personOut));
  });
  const findMember = (orgId: string, userId: string) => {
    const t = db.memberships.find((x) => x.org_id === orgId && x.user_id === userId);
    if (!t) throw notFound();
    return t;
  };
  const owners = (orgId: string) => db.memberships.filter((x) => x.org_id === orgId && x.role === 'owner').length;
  on('PATCH', `${ORG}/people/([^/]+)`, (c) => {
    const { orgId, m, user } = member(c);
    const t = findMember(orgId, c.params[1]);
    need(m, 'admin');
    const role = c.body.role as Role;
    if (!ROLES.includes(role)) throw invalid('Unknown role.');
    if (m.role !== 'owner' && (t.role === 'owner' || role === 'owner')) throw forbidden('Only an owner can change an owner or make someone an owner.');
    if (t.role === 'owner' && role !== 'owner' && owners(orgId) <= 1) throw conflict('A company needs at least one owner.');
    const from = t.role;
    t.role = role;
    const tu = db.users.find((u) => u.id === t.user_id)!;
    audit(orgId, 'person.role_changed', user, { type: 'person', id: tu.id, label: tu.email }, { from, to: role });
    return ok(personOut(t));
  });
  on('DELETE', `${ORG}/people/([^/]+)`, (c) => {
    const { orgId, m, user } = member(c);
    const t = findMember(orgId, c.params[1]);
    need(m, 'admin');
    if (t.role === 'owner' && m.role !== 'owner') throw forbidden('Only an owner can remove an owner.');
    if (t.role === 'owner' && owners(orgId) <= 1) throw conflict('A company needs at least one owner.');
    db.memberships = db.memberships.filter((x) => x !== t);
    const tu = db.users.find((u) => u.id === t.user_id)!;
    audit(orgId, 'person.removed', user, { type: 'person', id: tu.id, label: tu.email });
    return empty();
  });
  on('GET', `${ORG}/invites`, (c) => {
    const { orgId, m } = member(c);
    need(m, 'admin');
    return ok(db.invites.filter((i) => i.org_id === orgId).map(({ org_id: _o, token: _t, ...i }) => (void _o, void _t, i)));
  });
  on('POST', `${ORG}/invites`, (c) => {
    const { orgId, m, user } = member(c);
    need(m, 'admin');
    const email = str(c.body.email).toLowerCase();
    const role = c.body.role as Role;
    if (!isEmail(email)) throw invalid('Enter a valid email address.');
    if (!ROLES.includes(role)) throw invalid('Unknown role.');
    if (role === 'owner' && m.role !== 'owner') throw forbidden('Only an owner can invite an owner.');
    const existing = db.users.find((u) => u.email === email);
    if (existing && db.memberships.some((x) => x.org_id === orgId && x.user_id === existing.id)) throw conflict(`${email} is already in this company.`);
    if (db.invites.some((i) => i.org_id === orgId && i.email === email)) throw conflict(`${email} already has an invite.`);
    const inv = { id: ids.uuid(), org_id: orgId, email, role, invited_by: user.email, expires_at: iso(now() + 7 * 86_400_000), token: ids.secret(32) };
    db.invites.push(inv);
    db.outbox.push({ to: email, subject: `Join ${db.orgs.find((o) => o.id === orgId)!.name} on Rconn`, link: `/admin/invite?token=${inv.token}` });
    audit(orgId, 'person.invited', user, { type: 'person', id: inv.id, label: email }, { role });
    const { org_id: _o, token: _t, ...out } = inv;
    void _o;
    void _t;
    return ok(out, 201);
  });
  on('DELETE', `${ORG}/invites/([^/]+)`, (c) => {
    const { orgId, m } = member(c);
    const inv = db.invites.find((i) => i.org_id === orgId && i.id === c.params[1]);
    if (!inv) throw notFound();
    need(m, 'admin');
    db.invites = db.invites.filter((i) => i !== inv);
    return empty();
  });

  // ---------- Groups and policies ----------
  const groupOut = (g: { id: string; name: string; org_id: string }) => ({
    id: g.id,
    name: g.name,
    computers: db.computers.filter((x) => x.org_id === g.org_id && x.group_id === g.id && !x.revoked_at).length,
  });
  on('GET', `${ORG}/groups`, (c) => {
    const { orgId } = member(c);
    return ok(db.groups.filter((g) => g.org_id === orgId).map(groupOut));
  });
  const groupName = (orgId: string, name: string, except?: string) => {
    if (!name) throw invalid('Enter a group name.');
    if (db.groups.some((g) => g.org_id === orgId && g.id !== except && g.name.toLowerCase() === name.toLowerCase()))
      throw conflict(`A group called ${name} already exists.`);
  };
  on('POST', `${ORG}/groups`, (c) => {
    const { orgId, m } = member(c);
    need(m, 'admin');
    const name = str(c.body.name);
    groupName(orgId, name);
    const g = { id: ids.uuid(), org_id: orgId, name };
    db.groups.push(g);
    db.policies[`${orgId}:${g.id}`] = { ...(db.policies[`${orgId}:default`] ?? DEFAULT_POLICY) };
    return ok(groupOut(g), 201);
  });
  on('PATCH', `${ORG}/groups/([^/]+)`, (c) => {
    const { orgId, m } = member(c);
    const g = groupOf(orgId, c.params[1])!;
    need(m, 'admin');
    const name = str(c.body.name);
    groupName(orgId, name, g.id);
    g.name = name;
    return ok(groupOut(g));
  });
  on('DELETE', `${ORG}/groups/([^/]+)`, (c) => {
    const { orgId, m } = member(c);
    const g = groupOf(orgId, c.params[1])!;
    need(m, 'admin');
    db.groups = db.groups.filter((x) => x !== g);
    db.computers.forEach((x) => {
      if (x.group_id === g.id) x.group_id = null;
    });
    db.tokens.forEach((t) => {
      if (t.group_id === g.id) t.group_id = null;
    });
    delete db.policies[`${orgId}:${g.id}`];
    return empty();
  });
  const policyKey = (orgId: string, groupId: string | null) => `${orgId}:${groupId ?? 'default'}`;
  on('GET', `${ORG}/groups/([^/]+)/policy`, (c) => {
    const { orgId } = member(c);
    const g = groupOf(orgId, c.params[1])!;
    return ok(db.policies[policyKey(orgId, g.id)] ?? DEFAULT_POLICY);
  });
  on('PUT', `${ORG}/groups/([^/]+)/policy`, (c) => {
    const { orgId, m, user } = member(c);
    const g = groupOf(orgId, c.params[1])!;
    need(m, 'admin');
    const p = validPolicy(c.body);
    db.policies[policyKey(orgId, g.id)] = p;
    audit(orgId, 'policy.changed', user, { type: 'group', id: g.id, label: g.name });
    return ok(p);
  });
  on('GET', `${ORG}/policy/default`, (c) => {
    const { orgId } = member(c);
    return ok(db.policies[policyKey(orgId, null)] ?? DEFAULT_POLICY);
  });
  on('PUT', `${ORG}/policy/default`, (c) => {
    const { orgId, m, user } = member(c);
    need(m, 'admin');
    const p = validPolicy(c.body);
    db.policies[policyKey(orgId, null)] = p;
    audit(orgId, 'policy.changed', user, { type: 'group', id: 'default', label: 'Company default' });
    return ok(p);
  });

  // ---------- Audit ----------
  on('GET', `${ORG}/audit`, (c) => {
    const { orgId, m } = member(c);
    need(m, 'admin');
    const all = filterAudit(orgId, c.req.query);
    const start = Number(c.req.query.get('cursor') ?? 0) || 0;
    const limit = Math.min(Math.max(Number(c.req.query.get('limit') ?? 50) || 50, 1), 200);
    const items = all.slice(start, start + limit).map(strip);
    return ok({ items, next: start + limit < all.length ? String(start + limit) : null });
  });
  on('GET', `${ORG}/audit\\.csv`, (c) => {
    const { orgId, m } = member(c);
    need(m, 'admin');
    const rows = filterAudit(orgId, c.req.query).map((e) =>
      [e.at, e.kind, e.actor.type, e.actor.label, e.target?.type ?? '', e.target?.label ?? '', e.ip ?? '', JSON.stringify(e.detail)]
        .map(csvCell)
        .join(','),
    );
    const text = ['at,kind,actor_type,actor,target_type,target,ip,detail', ...rows].join('\n') + '\n';
    return { status: 200, text, contentType: 'text/csv; charset=utf-8' };
  });

  // ---------- SSO ----------
  const ssoOut = (orgId: string) => {
    const s = db.sso[orgId];
    return { enabled: s.enabled, issuer: s.issuer, client_id: s.client_id, domains: s.domains, auto_join_role: s.auto_join_role };
  };
  on('GET', `${ORG}/sso`, (c) => {
    const { orgId, m } = member(c);
    need(m, 'owner');
    return ok(ssoOut(orgId));
  });
  on('PUT', `${ORG}/sso`, (c) => {
    const { orgId, m, user } = member(c);
    need(m, 'owner');
    const s = db.sso[orgId];
    const issuer = str(c.body.issuer);
    const clientId = str(c.body.client_id);
    if (issuer && !/^https:\/\/[^\s]+$/.test(issuer)) throw invalid('The issuer must be an https:// URL.');
    const role = c.body.auto_join_role as Role;
    if (!ROLES.includes(role) || role === 'owner') throw invalid('Pick admin, technician or viewer.');
    const secret = str(c.body.client_secret);
    const enabled = c.body.enabled === true;
    if (enabled && (!issuer || !clientId || !(secret || s.client_secret))) throw invalid('Enter the issuer, client ID and secret before turning SSO on.');
    Object.assign(s, { issuer, client_id: clientId, auto_join_role: role, enabled });
    if (secret) s.client_secret = secret;
    audit(orgId, 'sso.changed', user, null, { enabled });
    return ok(ssoOut(orgId));
  });
  on('POST', `${ORG}/sso/domains`, (c) => {
    const { orgId, m } = member(c);
    need(m, 'owner');
    const domain = str(c.body.domain).toLowerCase();
    if (!/^([a-z0-9-]+\.)+[a-z]{2,}$/.test(domain)) throw invalid('Enter a domain like example.com.');
    for (const [o, s] of Object.entries(db.sso)) {
      if (s.domains.some((d) => d.domain === domain)) throw conflict(o === orgId ? 'That domain is already added.' : 'Another company has claimed that domain.');
    }
    const d = { domain, verified: false, txt_record: `rconn-verify=${ids.secret(16)}` };
    db.sso[orgId].domains.push(d);
    return ok(d, 201);
  });
  on('POST', `${ORG}/sso/domains/([^/]+)/verify`, (c) => {
    const { orgId, m } = member(c);
    need(m, 'owner');
    const d = db.sso[orgId].domains.find((x) => x.domain === decodeURIComponent(c.params[1]));
    if (!d) throw notFound();
    // The mock "finds" the TXT record for *.test domains only.
    if (d.domain.endsWith('.test')) d.verified = true;
    return ok(d);
  });

  function handle(req: MockRequest): MockResponse {
    let body: Body = {};
    if (req.body && typeof req.body === 'object') body = req.body as Body;
    if (req.method !== 'GET' && req.method !== 'DELETE' && req.body === undefined) {
      return { status: 400, json: { error: { code: 'invalid', message: 'JSON body required.' } } };
    }
    for (const [method, re, h] of routes) {
      if (method !== req.method) continue;
      const mm = re.exec(req.path);
      if (!mm) continue;
      try {
        const res = h({ req, params: mm.slice(1).map(decodeURIComponent), body });
        if (req.method !== 'GET') db.seq++;
        return res;
      } catch (e) {
        if (e instanceof HttpError) return { status: e.status, json: { error: { code: e.code, message: e.message } } };
        throw e;
      }
    }
    return { status: 404, json: { error: { code: 'not_found', message: `No route for ${req.method} ${req.path}` } } };
  }

  return { db, handle };
}
