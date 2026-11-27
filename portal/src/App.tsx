import { useState, type FormEvent, type ReactNode } from 'react';
import { BrowserRouter, Navigate, Route, Routes, useLocation, useNavigate } from 'react-router';
import { SessionProvider, preferredOrg, useSession } from './lib/session';
import { OrgLayout, Shell } from './components/Layout';
import { ErrorState, Loading, ToastProvider, errorMessage } from './components/ui';
import { createOrg } from './api/org';
import SignIn, { AuthCard, safeNext } from './pages/SignIn';
import Magic from './pages/Magic';
import InviteAccept from './pages/InviteAccept';
import Computers from './pages/Computers';
import Tokens from './pages/Tokens';
import People from './pages/People';
import Groups from './pages/Groups';
import Audit from './pages/Audit';
import Settings from './pages/Settings';
import Account from './pages/Account';

function RequireAuth({ children }: { children: ReactNode }) {
  const { state, refresh } = useSession();
  const loc = useLocation();
  if (state.status === 'loading') return <Loading />;
  if (state.status === 'error') return <ErrorState error={state.error} onRetry={() => void refresh()} />;
  if (state.status === 'signed-out') {
    const next = loc.pathname === '/' ? '' : `?next=${encodeURIComponent(loc.pathname + loc.search)}`;
    return <Navigate to={`/signin${next}`} replace />;
  }
  return <>{children}</>;
}

function PublicOnly({ children }: { children: ReactNode }) {
  const { state } = useSession();
  const loc = useLocation();
  if (state.status === 'loading') return <Loading />;
  if (state.status === 'signed-in') {
    return <Navigate to={safeNext(new URLSearchParams(loc.search).get('next'))} replace />;
  }
  return <>{children}</>;
}

function Home() {
  const { me, refresh } = useSession();
  const nav = useNavigate();
  const [name, setName] = useState('');
  const [err, setErr] = useState<string | null>(null);
  const m = me ? preferredOrg(me.memberships) : undefined;
  if (m) return <Navigate to={`/o/${m.org_id}/computers`} replace />;
  const create = async (e: FormEvent) => {
    e.preventDefault();
    setErr(null);
    try {
      const o = await createOrg(name.trim());
      await refresh();
      nav(`/o/${o.id}/computers`, { replace: true });
    } catch (e2) {
      setErr(errorMessage(e2));
    }
  };
  return (
    <AuthCard>
      <h1>You’re not in a company yet</h1>
      <p>Ask an admin to invite you, or create a company for your team.</p>
      <form onSubmit={create}>
        <div className="field">
          <label htmlFor="new-org">Company name</label>
          <input id="new-org" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        {err && <div className="alert">{err}</div>}
        <button className="btn btn-block" disabled={!name.trim()}>
          Create company
        </button>
      </form>
    </AuthCard>
  );
}

function AccountPage() {
  const { me } = useSession();
  return (
    <Shell current={me ? (preferredOrg(me.memberships) ?? null) : null}>
      <Account />
    </Shell>
  );
}

function NotFound() {
  return (
    <Shell current={null}>
      <ErrorState error={{ message: 'There is no page here.' }} />
    </Shell>
  );
}

export default function App() {
  return (
    <BrowserRouter basename={import.meta.env.BASE_URL.replace(/\/$/, '')}>
      <ToastProvider>
        <SessionProvider>
          <Routes>
            <Route
              path="/signin"
              element={
                <PublicOnly>
                  <SignIn />
                </PublicOnly>
              }
            />
            <Route path="/magic" element={<Magic />} />
            <Route path="/invite" element={<InviteAccept />} />
            <Route
              path="/"
              element={
                <RequireAuth>
                  <Home />
                </RequireAuth>
              }
            />
            <Route
              path="/account"
              element={
                <RequireAuth>
                  <AccountPage />
                </RequireAuth>
              }
            />
            <Route
              path="/o/:orgId"
              element={
                <RequireAuth>
                  <OrgLayout />
                </RequireAuth>
              }
            >
              <Route index element={<Navigate to="computers" replace />} />
              <Route path="computers" element={<Computers />} />
              <Route path="computers/tokens" element={<Tokens />} />
              <Route path="people" element={<People />} />
              <Route path="groups" element={<Groups />} />
              <Route path="audit" element={<Audit />} />
              <Route path="settings" element={<Settings />} />
            </Route>
            <Route
              path="*"
              element={
                <RequireAuth>
                  <NotFound />
                </RequireAuth>
              }
            />
          </Routes>
        </SessionProvider>
      </ToastProvider>
    </BrowserRouter>
  );
}
