import { api, orgPath } from './http';

export interface Group {
  id: string;
  name: string;
  /** Number of computers in the group. */
  computers: number;
}

export type Day = 'mon' | 'tue' | 'wed' | 'thu' | 'fri' | 'sat' | 'sun';
export const DAYS: Day[] = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];

export interface AllowedHours {
  /** IANA zone, e.g. "America/Chicago". */
  timezone: string;
  days: Day[];
  /** "HH:MM", 24-hour. `end` earlier than `start` means the window crosses midnight. */
  start: string;
  end: string;
}

export interface Policy {
  clipboard: boolean;
  file_transfer: boolean;
  audio: boolean;
  require_approval: boolean;
  show_indicator: boolean;
  allow_access_password: boolean;
  /** null = sessions allowed at any time. */
  allowed_hours: AllowedHours | null;
}

export const DEFAULT_POLICY: Policy = {
  clipboard: true,
  file_transfer: true,
  audio: true,
  require_approval: false,
  show_indicator: true,
  allow_access_password: true,
  allowed_hours: null,
};

export const listGroups = (orgId: string) => api<Group[]>('GET', orgPath(orgId, '/groups'));
export const createGroup = (orgId: string, name: string) => api<Group>('POST', orgPath(orgId, '/groups'), { name });
export const renameGroup = (orgId: string, id: string, name: string) =>
  api<Group>('PATCH', orgPath(orgId, `/groups/${encodeURIComponent(id)}`), { name });
export const deleteGroup = (orgId: string, id: string) =>
  api<void>('DELETE', orgPath(orgId, `/groups/${encodeURIComponent(id)}`));

/** `groupId` null = the company default (applies to ungrouped computers). */
export const getPolicy = (orgId: string, groupId: string | null) =>
  api<Policy>('GET', orgPath(orgId, groupId ? `/groups/${encodeURIComponent(groupId)}/policy` : '/policy/default'));
export const putPolicy = (orgId: string, groupId: string | null, policy: Policy) =>
  api<Policy>(
    'PUT',
    orgPath(orgId, groupId ? `/groups/${encodeURIComponent(groupId)}/policy` : '/policy/default'),
    policy,
  );
