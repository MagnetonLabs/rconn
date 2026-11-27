import { api, orgPath } from './http';
import type { Role } from './auth';

export interface SsoDomain {
  domain: string;
  verified: boolean;
  /** Value of the TXT record to publish at the domain's apex. */
  txt_record: string;
}

export interface SsoConfig {
  enabled: boolean;
  issuer: string;
  client_id: string;
  domains: SsoDomain[];
  auto_join_role: Role;
}

export interface SsoUpdate {
  issuer: string;
  client_id: string;
  /** Omit to keep the stored secret. */
  client_secret?: string;
  auto_join_role: Role;
  enabled: boolean;
}

export const getSso = (orgId: string) => api<SsoConfig>('GET', orgPath(orgId, '/sso'));
export const putSso = (orgId: string, u: SsoUpdate) => {
  const body: Record<string, unknown> = {
    issuer: u.issuer,
    client_id: u.client_id,
    auto_join_role: u.auto_join_role,
    enabled: u.enabled,
  };
  if (u.client_secret) body.client_secret = u.client_secret;
  return api<SsoConfig>('PUT', orgPath(orgId, '/sso'), body);
};
export const addDomain = (orgId: string, domain: string) =>
  api<SsoDomain>('POST', orgPath(orgId, '/sso/domains'), { domain });
export const verifyDomain = (orgId: string, domain: string) =>
  api<SsoDomain>('POST', orgPath(orgId, `/sso/domains/${encodeURIComponent(domain)}/verify`));
