import { useState, type FormEvent } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import * as auth from '../api/auth';
import { ApiError } from '../api/http';
import { useSession } from '../lib/session';
import { errorMessage } from '../components/ui';
import { AuthCard, takeNext } from './SignIn';

// The GET only shows a button: mail scanners open links, and only the POST spends the token.
export default function Magic() {
  const [params] = useSearchParams();
  const token = params.get('token') ?? '';
  const { setMe } = useSession();
  const nav = useNavigate();
  const [needsCode, setNeedsCode] = useState(false);
  const [code, setCode] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dead, setDead] = useState(!token);

  const redeem = async (withCode?: string) => {
    setBusy(true);
    setError(null);
    try {
      const me = await auth.redeemMagic(token, withCode);
      setMe(me);
      nav(takeNext(), { replace: true });
    } catch (e) {
      if (e instanceof ApiError && e.code === 'needs_code') setNeedsCode(true);
      else if (e instanceof ApiError && e.status === 400 && withCode) setError(e.message);
      else if (e instanceof ApiError && e.status === 400) setDead(true);
      else setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const submitCode = (e: FormEvent) => {
    e.preventDefault();
    if (code.length === 6) void redeem(code);
  };

  if (dead) {
    return (
      <AuthCard>
        <h1>This link has expired</h1>
        <p>Sign-in links work once, for 15 minutes. Ask for a new one.</p>
        <Link className="btn" to="/signin">
          Back to sign in
        </Link>
      </AuthCard>
    );
  }

  if (needsCode) {
    return (
      <AuthCard>
        <h1>Enter the code from the email</h1>
        <p className="muted">This link was opened in a different browser from the one that asked for it.</p>
        <form onSubmit={submitCode}>
          <div className="field">
            <label htmlFor="code">6-digit code</label>
            <input
              id="code"
              className="code-input"
              inputMode="numeric"
              autoComplete="one-time-code"
              pattern="[0-9]{6}"
              maxLength={6}
              autoFocus
              value={code}
              aria-invalid={error ? true : undefined}
              onChange={(e) => setCode(e.target.value.replace(/\D/g, '').slice(0, 6))}
            />
          </div>
          {error && (
            <div className="alert" role="alert">
              {error}
            </div>
          )}
          <button className="btn btn-block" disabled={busy || code.length !== 6}>
            {busy ? 'Signing in…' : 'Sign in'}
          </button>
        </form>
      </AuthCard>
    );
  }

  return (
    <AuthCard>
      <h1>Sign in to Rconn</h1>
      <p>Continue to the admin portal.</p>
      {error && (
        <div className="alert" role="alert">
          {error}
        </div>
      )}
      <button className="btn btn-block" onClick={() => void redeem()} disabled={busy} autoFocus>
        {busy ? 'Signing in…' : 'Sign in'}
      </button>
    </AuthCard>
  );
}
