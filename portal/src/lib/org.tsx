import { createContext, useContext } from 'react';
import type { Membership } from '../api/auth';
import { can, type Action } from './roles';

/** The company the current page is about, from the URL, with the caller's role in it. */
export const OrgCtx = createContext<Membership | null>(null);

export function useOrg(): Membership {
  const m = useContext(OrgCtx);
  if (!m) throw new Error('useOrg outside an org route');
  return m;
}

export function useCan(action: Action): boolean {
  return can(useContext(OrgCtx)?.role, action);
}
