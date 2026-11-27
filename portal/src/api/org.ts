import { api, orgPath } from './http';

export interface Org {
  id: string;
  name: string;
  created_at: string;
  plan: string;
}

export const getOrg = (orgId: string) => api<Org>('GET', orgPath(orgId, ''));
export const renameOrg = (orgId: string, name: string) => api<Org>('PATCH', orgPath(orgId, ''), { name });
export const createOrg = (name: string) => api<Org>('POST', '/orgs', { name });
