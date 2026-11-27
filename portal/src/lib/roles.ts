import type { Role } from '../api/auth';

// Mirrors the role table in the API contract. The server is the authority; this only decides
// what the UI offers.

const RANK: Record<Role, number> = { owner: 3, admin: 2, technician: 1, viewer: 0 };

export type Action =
  | 'computers.manage' // rename, move, revoke, enrollment tokens
  | 'groups.manage' // groups and policies
  | 'people.manage' // invite, change role, remove
  | 'audit.view'
  | 'sso.manage'
  | 'org.rename';

const MIN_ROLE: Record<Action, Role> = {
  'computers.manage': 'admin',
  'groups.manage': 'admin',
  'people.manage': 'admin',
  'audit.view': 'admin',
  'sso.manage': 'owner',
  'org.rename': 'owner',
};

export function atLeast(role: Role | undefined, min: Role): boolean {
  return role !== undefined && RANK[role] >= RANK[min];
}

export function can(role: Role | undefined, action: Action): boolean {
  return atLeast(role, MIN_ROLE[action]);
}

/** Roles `actor` may give to someone (in an invite or a role change). Admins can't create owners. */
export function assignableRoles(actor: Role | undefined): Role[] {
  if (actor === 'owner') return ['owner', 'admin', 'technician', 'viewer'];
  if (actor === 'admin') return ['admin', 'technician', 'viewer'];
  return [];
}

/**
 * Whether `actor` may change the role of, or remove, a member whose role is `target`.
 * Admins can't touch owners. Nobody changes their own role or removes themself here, and the
 * last owner always stays an owner.
 */
export function canManageMember(
  actor: Role | undefined,
  target: Role,
  opts: { self: boolean; ownerCount: number },
): boolean {
  if (!can(actor, 'people.manage')) return false;
  if (opts.self) return false;
  if (target === 'owner' && actor !== 'owner') return false;
  if (target === 'owner' && opts.ownerCount <= 1) return false;
  return true;
}

export const ROLE_LABEL: Record<Role, string> = {
  owner: 'Owner',
  admin: 'Admin',
  technician: 'Technician',
  viewer: 'Viewer',
};
