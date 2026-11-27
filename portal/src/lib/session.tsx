import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import * as auth from '../api/auth';
import type { Me, Membership } from '../api/auth';
import { ApiError, UNAUTHORIZED_EVENT } from '../api/http';

type State = { status: 'loading' } | { status: 'signed-out' } | { status: 'signed-in'; me: Me } | { status: 'error'; error: unknown };

interface SessionCtx {
  state: State;
  me: Me | null;
  setMe: (me: Me) => void;
  signOut: () => Promise<void>;
  refresh: () => Promise<void>;
}

const Ctx = createContext<SessionCtx | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<State>({ status: 'loading' });

  const refresh = useCallback(async () => {
    try {
      const me = await auth.getMe();
      setState({ status: 'signed-in', me });
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) setState({ status: 'signed-out' });
      else setState({ status: 'error', error: e });
    }
  }, []);

  useEffect(() => {
    void refresh();
    const onUnauth = () => setState({ status: 'signed-out' });
    window.addEventListener(UNAUTHORIZED_EVENT, onUnauth);
    return () => window.removeEventListener(UNAUTHORIZED_EVENT, onUnauth);
  }, [refresh]);

  const signOut = useCallback(async () => {
    try {
      await auth.logout();
    } finally {
      setState({ status: 'signed-out' });
    }
  }, []);

  const value = useMemo<SessionCtx>(
    () => ({
      state,
      me: state.status === 'signed-in' ? state.me : null,
      setMe: (me) => setState({ status: 'signed-in', me }),
      signOut,
      refresh,
    }),
    [state, signOut, refresh],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useSession(): SessionCtx {
  const c = useContext(Ctx);
  if (!c) throw new Error('useSession outside SessionProvider');
  return c;
}

const LAST_ORG = 'rconn-admin-org';
export function rememberOrg(id: string) {
  try {
    localStorage.setItem(LAST_ORG, id);
  } catch {
    /* ignore */
  }
}
export function preferredOrg(memberships: Membership[]): Membership | undefined {
  let last: string | null = null;
  try {
    last = localStorage.getItem(LAST_ORG);
  } catch {
    /* ignore */
  }
  return memberships.find((m) => m.org_id === last) ?? memberships[0];
}
