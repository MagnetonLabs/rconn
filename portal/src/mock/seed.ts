// Seed data for the mock backend: 2 companies, 25 computers, 6 people, 5 groups, 200 audit events.
// Deterministic (seeded PRNG) so tests and screenshots are stable; times are relative to `now`.

import type { Role } from '../api/auth';
import type { Computer, EnrollmentToken } from '../api/computers';
import type { Invite } from '../api/people';
import type { Policy } from '../api/policies';
import type { AuditEvent } from '../api/audit';
import type { SsoDomain } from '../api/sso';
import type { Org } from '../api/org';
import type { Passkey } from '../api/auth';
import { DEFAULT_POLICY } from '../api/policies';

export const DEMO_PASSWORD = 'correct-horse';
export const CURRENT_VERSION = '0.1.11';

export interface DbUser {
  id: string;
  email: string;
  name: string;
  password: string | null;
}
export interface DbMembership {
  org_id: string;
  user_id: string;
  role: Role;
  joined_at: string;
  last_active: string | null;
}
export interface DbGroup {
  id: string;
  org_id: string;
  name: string;
}
export type DbComputer = Computer & { org_id: string };
export type DbToken = EnrollmentToken & { org_id: string };
export type DbInvite = Invite & { org_id: string; token: string };
export type DbAudit = AuditEvent & { org_id: string };
export interface DbSso {
  enabled: boolean;
  issuer: string;
  client_id: string;
  client_secret: string | null;
  auto_join_role: Role;
  domains: SsoDomain[];
}
export interface DbMagic {
  token: string;
  email: string;
  code: string;
  /** false = opened in a different browser than the one that asked: needs the 6-digit code. */
  bound: boolean;
  used: boolean;
  expires_at: number;
}
export interface MailItem {
  to: string;
  subject: string;
  link: string;
  code?: string;
}

export interface Db {
  now: number;
  users: DbUser[];
  orgs: Org[];
  memberships: DbMembership[];
  groups: DbGroup[];
  /** key: `${org_id}:${group_id | 'default'}` */
  policies: Record<string, Policy>;
  computers: DbComputer[];
  tokens: DbToken[];
  invites: DbInvite[];
  audit: DbAudit[];
  sso: Record<string, DbSso>;
  passkeys: (Passkey & { user_id: string })[];
  sessions: Record<string, string>;
  magic: DbMagic[];
  outbox: MailItem[];
  failures: Record<string, number>;
  seq: number;
}

function mulberry32(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function makeIds(seed: number) {
  const rnd = mulberry32(seed);
  const hex = (n: number) =>
    Array.from({ length: n }, () => Math.floor(rnd() * 16).toString(16)).join('');
  return {
    rnd,
    uuid: () => `${hex(8)}-${hex(4)}-4${hex(3)}-${'89ab'[Math.floor(rnd() * 4)]}${hex(3)}-${hex(12)}`,
    secret: (n = 32) =>
      Array.from({ length: n }, () => 'abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'[Math.floor(rnd() * 57)]).join(''),
    digits: (n: number) => Array.from({ length: n }, () => Math.floor(rnd() * 10)).join(''),
    pick: <T,>(xs: readonly T[]) => xs[Math.floor(rnd() * xs.length)],
  };
}

const iso = (t: number) => new Date(t).toISOString();
const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

export const ORG_A = '6f1d2c9a-3b7e-4c1a-9e55-0a1b2c3d4e5f';
export const ORG_B = 'b2e4f6a8-1c3d-4e5f-8a7b-9c0d1e2f3a4b';

export function seed(now = Date.now()): Db {
  const ids = makeIds(20261127);
  const { uuid, rnd, pick, digits } = ids;

  const orgs: Org[] = [
    { id: ORG_A, name: 'Northwind Dental', created_at: iso(now - 210 * DAY), plan: 'business' },
    { id: ORG_B, name: 'Harbor Accounting', created_at: iso(now - 95 * DAY), plan: 'team' },
  ];

  const mk = (email: string, name: string): DbUser => ({ id: uuid(), email, name, password: DEMO_PASSWORD });
  const dana = mk('dana@northwind.test', 'Dana Reyes');
  const sam = mk('sam@northwind.test', 'Sam Okafor');
  const priya = mk('priya@fixit.test', 'Priya Natarajan');
  const leo = mk('leo@northwind.test', 'Leo Martins');
  const grace = mk('grace@harbor.test', 'Grace Kim');
  const tom = mk('tom@harbor.test', 'Tom Becker');
  const users = [dana, sam, priya, leo, grace, tom];

  const m = (org_id: string, u: DbUser, role: Role, joinedDays: number, activeMin: number | null): DbMembership => ({
    org_id,
    user_id: u.id,
    role,
    joined_at: iso(now - joinedDays * DAY),
    last_active: activeMin === null ? null : iso(now - activeMin * MIN),
  });
  const memberships: DbMembership[] = [
    m(ORG_A, dana, 'owner', 210, 4),
    m(ORG_A, sam, 'admin', 180, 95),
    m(ORG_A, priya, 'technician', 120, 33),
    m(ORG_A, leo, 'viewer', 40, 60 * 26),
    m(ORG_B, grace, 'owner', 95, 12),
    m(ORG_B, priya, 'technician', 80, 33),
    m(ORG_B, tom, 'viewer', 30, null),
    m(ORG_B, dana, 'admin', 60, 4),
  ];

  const g = (org_id: string, name: string): DbGroup => ({ id: uuid(), org_id, name });
  const front = g(ORG_A, 'Front desk');
  const ops = g(ORG_A, 'Operatories');
  const servers = g(ORG_A, 'Servers');
  const hws = g(ORG_B, 'Workstations');
  const hlap = g(ORG_B, 'Laptops');
  const groups = [front, ops, servers, hws, hlap];

  const policies: Record<string, Policy> = {
    [`${ORG_A}:default`]: { ...DEFAULT_POLICY },
    [`${ORG_A}:${front.id}`]: { ...DEFAULT_POLICY, audio: false },
    [`${ORG_A}:${ops.id}`]: {
      ...DEFAULT_POLICY,
      file_transfer: false,
      require_approval: true,
      allowed_hours: { timezone: 'America/Chicago', days: ['mon', 'tue', 'wed', 'thu', 'fri'], start: '07:30', end: '18:00' },
    },
    [`${ORG_A}:${servers.id}`]: { ...DEFAULT_POLICY, clipboard: false, audio: false, allow_access_password: false },
    [`${ORG_B}:default`]: { ...DEFAULT_POLICY },
    [`${ORG_B}:${hws.id}`]: { ...DEFAULT_POLICY },
    [`${ORG_B}:${hlap.id}`]: { ...DEFAULT_POLICY, require_approval: true },
  };

  const shortCode = () => `${digits(3)} ${digits(3)} ${digits(3)}`.replace(/^0/, '8');
  const computers: DbComputer[] = [];
  type Spec = [name: string, host: string, os: string, group: DbGroup | null, version?: string, state?: 'online' | 'offline' | 'revoked'];
  const specA: Spec[] = [
    ['Front desk 1', 'FRONTDESK-01', 'Windows 11 Pro 24H2', front],
    ['Front desk 2', 'FRONTDESK-02', 'Windows 11 Pro 24H2', front],
    ['Check-in kiosk', 'KIOSK-LOBBY', 'Windows 11 IoT Enterprise', front, '0.1.9'],
    ['Billing', 'BILLING-PC', 'Windows 10 Pro 22H2', front, '0.1.10', 'offline'],
    ['Op 1', 'OP-01', 'Windows 11 Pro 24H2', ops],
    ['Op 2', 'OP-02', 'Windows 11 Pro 24H2', ops],
    ['Op 3', 'OP-03', 'Windows 11 Pro 23H2', ops, '0.1.9'],
    ['Op 4', 'OP-04', 'Windows 11 Pro 24H2', ops, undefined, 'offline'],
    ['Hygiene 1', 'HYG-01', 'Windows 11 Pro 24H2', ops],
    ['Imaging', 'PANO-XRAY', 'Windows 10 Enterprise LTSC', ops, '0.1.8', 'offline'],
    ['Practice server', 'NWD-SRV01', 'Windows Server 2022', servers],
    ['Backup NAS bridge', 'nwd-backup', 'Ubuntu 24.04 LTS', servers],
    ['Dr. Patel MacBook', 'Patels-MacBook-Pro', 'macOS 15.5', null],
    ['Office manager', 'OFFICE-MGR', 'macOS 15.4', null, '0.1.10'],
    ['Old reception PC', 'RECEPTION-OLD', 'Windows 10 Pro 21H2', null, '0.1.6', 'revoked'],
    ['Lab tablet', 'LAB-SURFACE', 'Windows 11 Home 24H2', null, undefined, 'offline'],
    ['Sterilization room', 'STERI-01', 'Ubuntu 24.04 LTS', ops],
  ];
  const specB: Spec[] = [
    ['Grace desktop', 'HARBOR-GK', 'Windows 11 Pro 24H2', hws],
    ['Reception', 'HARBOR-RECEP', 'Windows 11 Pro 24H2', hws],
    ['Tax season 1', 'HARBOR-TAX1', 'Windows 11 Pro 24H2', hws, '0.1.10'],
    ['Tax season 2', 'HARBOR-TAX2', 'Windows 11 Pro 24H2', hws, undefined, 'offline'],
    ['Tom laptop', 'Toms-MacBook-Air', 'macOS 15.5', hlap],
    ['Partner laptop', 'HARBOR-LT-07', 'Windows 11 Pro 24H2', hlap, '0.1.9', 'offline'],
    ['File server', 'harbor-files', 'Ubuntu 22.04 LTS', null],
    ['Scanner station', 'SCAN-01', 'Windows 10 Pro 22H2', null, '0.1.7', 'revoked'],
  ];
  const addAll = (org_id: string, specs: Spec[], enroller: DbUser) => {
    specs.forEach(([name, hostname, os, group, version = CURRENT_VERSION, state], i) => {
      const enrolledAt = now - (20 + Math.floor(rnd() * 150)) * DAY;
      const st = state ?? (rnd() < 0.85 ? 'online' : 'offline');
      const lastSeen =
        st === 'online' ? now - Math.floor(rnd() * 50) * 1000 : now - (1 + Math.floor(rnd() * 60 * 24 * 9)) * MIN;
      computers.push({
        id: uuid(),
        org_id,
        short_code: i === 0 && org_id === ORG_A ? '880 615 746' : shortCode(),
        name,
        hostname,
        os,
        version,
        group_id: group?.id ?? null,
        online: st === 'online',
        last_seen: iso(lastSeen),
        enrolled_at: iso(enrolledAt),
        enrolled_by: enroller.email,
        revoked_at: st === 'revoked' ? iso(now - 6 * DAY) : null,
        needs_update: version !== CURRENT_VERSION,
      });
    });
  };
  addAll(ORG_A, specA, sam);
  addAll(ORG_B, specB, grace);
  // A host from 0.1.x: connected, but never reported its OS or version.
  computers.push({
    id: 'b0d1e5c0-0000-4000-8000-000000000001',
    org_id: ORG_A,
    short_code: '104 220 381',
    name: 'WAITING-ROOM',
    hostname: 'WAITING-ROOM',
    os: null,
    version: null,
    group_id: null,
    online: false,
    last_seen: iso(now - 3 * DAY),
    enrolled_at: null,
    enrolled_by: null,
    revoked_at: null,
    needs_update: true,
  });

  const tokens: DbToken[] = [
    {
      id: uuid(),
      org_id: ORG_A,
      label: 'Operatory rollout',
      group_id: ops.id,
      expires_at: iso(now + 5 * DAY),
      uses: 3,
      max_uses: 10,
      created_by: sam.email,
      created_at: iso(now - 2 * DAY),
    },
    {
      id: uuid(),
      org_id: ORG_A,
      label: 'New hires',
      group_id: null,
      expires_at: iso(now + 20 * DAY),
      uses: 0,
      max_uses: null,
      created_by: dana.email,
      created_at: iso(now - 10 * DAY),
    },
    {
      id: uuid(),
      org_id: ORG_B,
      label: 'Laptops',
      group_id: hlap.id,
      expires_at: iso(now + 1 * DAY),
      uses: 1,
      max_uses: 5,
      created_by: grace.email,
      created_at: iso(now - 6 * DAY),
    },
  ];

  const invites: DbInvite[] = [
    {
      id: uuid(),
      org_id: ORG_A,
      email: 'maria@northwind.test',
      role: 'technician',
      invited_by: dana.email,
      expires_at: iso(now + 6 * DAY),
      token: 'invite-demo',
    },
  ];

  // 200 audit events: 150 for Northwind, 50 for Harbor, newest first.
  const audit: DbAudit[] = [];
  const ips = ['203.0.113.24', '198.51.100.7', '192.0.2.88', '203.0.113.150', '198.51.100.42'];
  const addEvents = (org_id: string, n: number, people: DbUser[]) => {
    const pcs = computers.filter((c) => c.org_id === org_id);
    let t = now - 3 * MIN;
    for (let i = 0; i < n; i++) {
      t -= Math.floor(rnd() * 5 * HOUR) + 5 * MIN;
      const u = pick(people);
      const pc = pick(pcs);
      const r = rnd();
      const actor = { type: 'user' as const, id: u.id, label: u.email };
      const host = { type: 'computer', id: pc.id, label: pc.name };
      let ev: Omit<DbAudit, 'id' | 'at' | 'org_id'>;
      if (r < 0.32) ev = { kind: 'session.start', actor, target: host, ip: pick(ips), detail: { session: digits(6) } };
      else if (r < 0.6)
        ev = { kind: 'session.end', actor, target: host, ip: pick(ips), detail: { duration_s: 60 + Math.floor(rnd() * 3000) } };
      else if (r < 0.68)
        ev = {
          kind: pick(['file.upload', 'file.download'] as const),
          actor,
          target: host,
          ip: pick(ips),
          detail: { file: pick(['claims-nov.xlsx', 'xray-0412.dcm', 'invoice.pdf', 'driver.zip']), bytes: Math.floor(rnd() * 9e6) },
        };
      else if (r < 0.8) ev = { kind: 'sign_in', actor, target: null, ip: pick(ips), detail: { method: pick(['password', 'magic_link', 'passkey']) } };
      else if (r < 0.85) ev = { kind: 'sign_in.failed', actor: { type: 'system', id: 'auth', label: u.email }, target: null, ip: pick(ips), detail: { reason: 'bad_password' } };
      else if (r < 0.88) ev = { kind: 'computer.renamed', actor, target: host, ip: pick(ips), detail: { from: pc.hostname, to: pc.name } };
      else if (r < 0.91) ev = { kind: 'policy.changed', actor, target: { type: 'group', id: 'g', label: pick(['Front desk', 'Operatories', 'Company default']) }, ip: pick(ips), detail: { field: pick(['clipboard', 'allowed_hours', 'require_approval']) } };
      else if (r < 0.94) ev = { kind: 'computer.enrolled', actor: { type: 'host', id: pc.id, label: pc.hostname ?? pc.name }, target: host, ip: pick(ips), detail: { via: 'enrollment_token', token_label: 'Operatory rollout' } };
      else if (r < 0.96) ev = { kind: 'token.created', actor, target: { type: 'token', id: 't', label: 'Operatory rollout' }, ip: pick(ips), detail: {} };
      else if (r < 0.98) ev = { kind: 'person.invited', actor, target: { type: 'person', id: 'p', label: 'maria@northwind.test' }, ip: pick(ips), detail: { role: 'technician' } };
      else ev = { kind: 'person.role_changed', actor, target: { type: 'person', id: 'p', label: pick(people).email }, ip: pick(ips), detail: { from: 'viewer', to: 'technician' } };
      audit.push({ id: uuid(), org_id, at: iso(t), ...ev });
    }
  };
  addEvents(ORG_A, 150, [dana, sam, priya]);
  addEvents(ORG_B, 50, [grace, priya, dana]);
  audit.sort((a, b) => (a.at < b.at ? 1 : -1));

  const sso: Record<string, DbSso> = {
    [ORG_A]: { enabled: false, issuer: '', client_id: '', client_secret: null, auto_join_role: 'viewer', domains: [] },
    [ORG_B]: {
      enabled: true,
      issuer: 'https://login.microsoftonline.com/4d2f1b7e-0000-4c8a-9f1e-harbor/v2.0',
      client_id: 'b7d0c1e2-5a6f-4b3c-8d9e-0f1a2b3c4d5e',
      client_secret: 'stored',
      auto_join_role: 'viewer',
      domains: [{ domain: 'harbor.test', verified: true, txt_record: 'rconn-verify=hb7Qm2xKp9' }],
    },
  };

  const passkeys = [
    { id: uuid(), user_id: dana.id, name: 'MacBook Touch ID', created_at: iso(now - 30 * DAY), last_used: iso(now - 4 * MIN) },
    { id: uuid(), user_id: dana.id, name: 'YubiKey 5C', created_at: iso(now - 90 * DAY), last_used: null },
  ];

  const magic: DbMagic[] = [
    // A link opened in another browser: the portal asks for the code from the email.
    { token: 'other-browser', email: dana.email, code: '246810', bound: false, used: false, expires_at: now + 15 * MIN },
  ];

  return {
    now,
    users,
    orgs,
    memberships,
    groups,
    policies,
    computers,
    tokens,
    invites,
    audit,
    sso,
    passkeys,
    sessions: {},
    magic,
    outbox: [],
    failures: {},
    seq: 0,
  };
}
