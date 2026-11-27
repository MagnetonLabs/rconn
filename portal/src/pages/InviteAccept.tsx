import { useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import * as auth from '../api/auth';
import { ApiError } from '../api/http';
import { useSession } from '../lib/session';
import { Loading, errorMessage } from '../components/ui';
import { AuthCard } from './SignIn';

export default function InviteAccept() {
  const [params] = useSearchParams();
  const token = params.get('token') ?? '';
  const { state, me, setMe, signOut } = useSession();
  const nav = useNavigate();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const here = `/invite?token=${encodeURIComponent(token)}`;

  if (!token) {
    return (
      <AuthCard>
        <h1>Invite link incomplete</h1>
        <p>Open the link from the invite email again.</p>
      </AuthCard>
    );
  }

  if (state.status === 'loading') {
    return (
      <AuthCard>
        <Loading />
      </AuthCard>
    );
  }

  if (!me) {
    return (
      <AuthCard>
        <h1>You’re invited</h1>
        <p>Sign in with the email address the invite was sent to. New to Rconn? Choose “Email me a sign-in link”.</p>
        <Link className="btn btn-block" to={`/signin?next=${encodeURIComponent(here)}`}>
          Sign in to accept
        </Link>
      </AuthCard>
    );
  }

  const accept = async () => {
    setBusy(true);
    setError(null);
    try {
      const before = new Set(me.memberships.map((m) => m.org_id));
      const next = await auth.acceptInvite(token);
      setMe(next);
      const joined = next.memberships.find((m) => !before.has(m.org_id)) ?? next.memberships[0];
      nav(joined ? `/o/${joined.org_id}/computers` : '/', { replace: true });
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) nav(`/signin?next=${encodeURIComponent(here)}`);
      else setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthCard>
      <h1>Join the company</h1>
      <p>
        You’re signed in as <strong>{me.user.email}</strong>.
      </p>
      {error && (
        <div className="alert" role="alert">
          {error}
        </div>
      )}
      <button className="btn btn-block" onClick={accept} disabled={busy} autoFocus>
        {busy ? 'Joining…' : 'Accept invite'}
      </button>
      <button
        className="btn btn-quiet btn-block"
        onClick={async () => {
          await signOut();
          nav(`/signin?next=${encodeURIComponent(here)}`);
        }}
      >
        Use a different account
      </button>
    </AuthCard>
  );
}
