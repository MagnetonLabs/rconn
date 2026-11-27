import { api, url } from './http';

export type Role = 'owner' | 'admin' | 'technician' | 'viewer';
export const ROLES: Role[] = ['owner', 'admin', 'technician', 'viewer'];

export interface User {
  id: string;
  email: string;
  name: string;
}

export interface Membership {
  org_id: string;
  org_name: string;
  role: Role;
}

export interface Me {
  user: User;
  memberships: Membership[];
}

export interface AuthOptions {
  password: boolean;
  magic_link: boolean;
  sso: { org_id: string; name: string } | null;
  passkey: boolean;
}

export interface Passkey {
  id: string;
  name: string;
  created_at: string;
  last_used: string | null;
}

export const getOptions = (email?: string) => api<AuthOptions>('GET', '/auth/options', undefined, { email });
export const signInPassword = (email: string, password: string) =>
  api<Me>('POST', '/auth/password', { email, password });
export const requestMagicLink = (email: string) => api<void>('POST', '/auth/magic-link', { email });
/** 400 with code `needs_code` means: ask for the 6-digit code from the email and call again with it. */
export const redeemMagic = (token: string, code?: string) =>
  api<Me>('POST', '/auth/magic', code ? { token, code } : { token });
export const logout = () => api<void>('POST', '/auth/logout');

export const getMe = () => api<Me>('GET', '/me');
export const updateMe = (name: string) => api<Me>('PATCH', '/me', { name });

/** Top-level, not org-scoped. Needs a session; 401 means sign in first. */
export const acceptInvite = (token: string) => api<Me>('POST', '/invites/accept', { token });

/** Full-page navigation to the company's identity provider. */
export const ssoStartUrl = (orgId: string) => url('/auth/sso/start', { org: orgId });

// Passkeys (wave 2). Options are WebAuthn JSON with base64url binary fields, wrapped in
// `{publicKey: …}` as webauthn-rs serialises them.
export interface CreationOptionsJSON {
  publicKey: Record<string, unknown>;
}
export interface RequestOptionsJSON {
  publicKey: Record<string, unknown>;
}
export const passkeyRegisterStart = (name: string) =>
  api<CreationOptionsJSON>('POST', '/auth/passkey/register/start', { name });
export const passkeyRegisterFinish = (name: string, credential: unknown) =>
  api<Passkey>('POST', '/auth/passkey/register/finish', { name, credential });
export const passkeyLoginStart = (email: string) =>
  api<RequestOptionsJSON>('POST', '/auth/passkey/login/start', { email });
export const passkeyLoginFinish = (credential: unknown) =>
  api<Me>('POST', '/auth/passkey/login/finish', { credential });
export const listPasskeys = () => api<Passkey[]>('GET', '/me/passkeys');
export const deletePasskey = (id: string) => api<void>('DELETE', `/me/passkeys/${encodeURIComponent(id)}`);
