import { api, orgPath } from './http';
import type { Role } from './auth';

export interface Person {
  user_id: string;
  email: string;
  name: string;
  role: Role;
  joined_at: string;
  last_active: string | null;
}

export interface Invite {
  id: string;
  email: string;
  role: Role;
  invited_by: string;
  expires_at: string;
}

export const listPeople = (orgId: string) => api<Person[]>('GET', orgPath(orgId, '/people'));
export const changeRole = (orgId: string, userId: string, role: Role) =>
  api<Person>('PATCH', orgPath(orgId, `/people/${encodeURIComponent(userId)}`), { role });
export const removePerson = (orgId: string, userId: string) =>
  api<void>('DELETE', orgPath(orgId, `/people/${encodeURIComponent(userId)}`));

export const listInvites = (orgId: string) => api<Invite[]>('GET', orgPath(orgId, '/invites'));
export const sendInvite = (orgId: string, email: string, role: Role) =>
  api<Invite>('POST', orgPath(orgId, '/invites'), { email, role });
export const cancelInvite = (orgId: string, id: string) =>
  api<void>('DELETE', orgPath(orgId, `/invites/${encodeURIComponent(id)}`));
