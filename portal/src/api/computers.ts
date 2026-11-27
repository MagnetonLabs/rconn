import { api, orgPath } from './http';

export type ComputerStatus = 'online' | 'offline' | 'revoked';

export interface Computer {
  id: string;
  short_code: string;
  /** The admin's name, else the hostname, else "Computer <short code>". */
  name: string;
  /** null: not reported yet (never connected) or by hosts that predate it (0.1.x). */
  hostname: string | null;
  os: string | null;
  version: string | null;
  group_id: string | null;
  online: boolean;
  last_seen: string | null;
  /** null for computers enrolled before the server had a database. */
  enrolled_at: string | null;
  enrolled_by: string | null;
  revoked_at: string | null;
  needs_update: boolean;
}

export interface ComputerFilter {
  q?: string;
  group_id?: string;
  status?: ComputerStatus;
}

export interface EnrollmentToken {
  id: string;
  label: string;
  group_id: string | null;
  expires_at: string;
  uses: number;
  max_uses: number | null;
  created_by: string;
  created_at: string;
}

export interface NewEnrollmentToken extends EnrollmentToken {
  /** The secret. Returned once, on create only. */
  token: string;
}

export interface CreateTokenInput {
  label: string;
  group_id?: string | null;
  expires_in_hours: number;
  max_uses?: number | null;
}

/** v1 has no "ungrouped" query value; the Computers page filters that client-side. */
export const listComputers = (orgId: string, f: ComputerFilter = {}) =>
  api<Computer[]>('GET', orgPath(orgId, '/computers'), undefined, { ...f });

/** `name: null` clears the admin's name (the hostname shows again). */
export const updateComputer = (orgId: string, id: string, patch: { name?: string | null; group_id?: string | null }) =>
  api<Computer>('PATCH', orgPath(orgId, `/computers/${encodeURIComponent(id)}`), patch);

export const revokeComputer = (orgId: string, id: string) =>
  api<Computer>('POST', orgPath(orgId, `/computers/${encodeURIComponent(id)}/revoke`));

export const listTokens = (orgId: string) => api<EnrollmentToken[]>('GET', orgPath(orgId, '/enrollment-tokens'));

export const createToken = (orgId: string, input: CreateTokenInput) => {
  const body: Record<string, unknown> = { label: input.label, expires_in_hours: input.expires_in_hours };
  if (input.group_id) body.group_id = input.group_id;
  if (input.max_uses) body.max_uses = input.max_uses;
  return api<NewEnrollmentToken>('POST', orgPath(orgId, '/enrollment-tokens'), body);
};

export const deleteToken = (orgId: string, id: string) =>
  api<void>('DELETE', orgPath(orgId, `/enrollment-tokens/${encodeURIComponent(id)}`));
