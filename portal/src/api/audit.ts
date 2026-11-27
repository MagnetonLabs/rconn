import { api, orgPath, url } from './http';

export const AUDIT_KINDS = [
  'session.start',
  'session.end',
  'file.upload',
  'file.download',
  'computer.enrolled',
  'computer.renamed',
  'computer.revoked',
  'person.invited',
  'person.role_changed',
  'person.removed',
  'policy.changed',
  'sign_in',
  'sign_in.failed',
  'sso.changed',
  'token.created',
  'token.deleted',
] as const;
export type AuditKind = (typeof AUDIT_KINDS)[number];

export interface AuditRef {
  type: string;
  id: string;
  label: string;
}

export interface AuditEvent {
  id: string;
  at: string;
  kind: AuditKind | string;
  actor: { type: 'user' | 'host' | 'system'; id: string; label: string };
  target: AuditRef | null;
  ip: string | null;
  detail: Record<string, unknown>;
}

export interface AuditPage {
  items: AuditEvent[];
  next: string | null;
}

export interface AuditFilter {
  /** RFC 3339. */
  from?: string;
  to?: string;
  q?: string;
  kind?: string;
}

export const listAudit = (orgId: string, f: AuditFilter, cursor?: string | null, limit = 50) =>
  api<AuditPage>('GET', orgPath(orgId, '/audit'), undefined, { ...f, cursor: cursor ?? undefined, limit });

/** A plain link: the browser downloads it with the session cookie. */
export const auditCsvUrl = (orgId: string, f: AuditFilter) => url(orgPath(orgId, '/audit.csv'), { ...f });
