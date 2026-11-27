import { useState, type FormEvent, type ReactNode } from 'react';
import { useNavigate, useSearchParams } from 'react-router';
import * as auth from '../api/auth';
import type { AuthOptions } from '../api/auth';
import { ApiError } from '../api/http';
import { useSession } from '../lib/session';
import { errorMessage } from '../components/ui';
import { getCredential, passkeysSupported } from '../lib/webauthn';

export function AuthCard({ children }: { children: ReactNode }) {
  return (
    <div className="auth-page">
      <main className="auth-card" id="main">
        <span className="brand">
          <img src={`${import.meta.env.BASE_URL}favicon.svg`} alt="" width="28" height="28" />
          Rconn
        </span>
        {children}
      </main>
    </div>
  );
}

const NEXT_KEY = 'rconn-admin-next';

/** Where to go after a magic link opened in this browser (the link itself carries no path). */
export function rememberNext(next: string) {
  try {
    if (next === '/') sessionStorage.removeItem(NEXT_KEY);
    else sessionStorage.setItem(NEXT_KEY, next);
  } catch {
    /* ignore */
  }
}
export function takeNext(): string {
  try {
    const v = sessionStorage.getItem(NEXT_KEY);
    sessionStorage.removeItem(NEXT_KEY);
    return safeNext(v);
  } catch {
    return '/';
  }
}

/** Only same-app paths are allowed after sign-in. */
export function safeNext(v: string | null): string {
  return v && v.startsWith('/') && !v.startsWith('//') ? v : '/';
}

/** What a failed single sign-on round trip (`/admin/signin?sso_error=…`) means. */
export function ssoErrorMessage(code: string | null): string | null {
  switch (code) {
    case null:
    case '':
      return null;
    case 'unavailable':
      return 'Single sign-on isn’t available for this company right now. Try another way to sign in.';
    case 'expired':
      return 'That single sign-on attempt expired or was already used. Try again.';
    case 'domain':
      return 'Your identity provider account isn’t at a domain this company has verified. Ask an owner to verify it, or sign in another way.';
    case 'rate_limited':
      return 'Too many attempts. Wait a few minutes and try again.';
    default:
      return 'Single sign-on didn’t complete. Try again, or sign in another way.';
  }
}

export default function SignIn() {
  const [params] = useSearchParams();
  const next = safeNext(params.get('next'));
  const { setMe } = useSession();
  const nav = useNavigate();
  const [email, setEmail] = useState(params.get('email') ?? '');
  const [options, setOptions] = useState<AuthOptions | null>(null);
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(() => ssoErrorMessage(params.get('sso_error')));
  const [sent, setSent] = useState(false);

  const done = (me: auth.Me) => {
    setMe(me);
    nav(next, { replace: true });
  };

  const run = async (what: string, fn: () => Promise<void>) => {
    setBusy(what);
    setError(null);
    try {
      await fn();
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) setError('That email and password don’t match.');
      else setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const submitEmail = (e: FormEvent) => {
    e.preventDefault();
    if (!email.trim()) return;
    void run('options', async () => setOptions(await auth.getOptions(email.trim())));
  };

  const submitPassword = (e: FormEvent) => {
    e.preventDefault();
    void run('password', async () => done(await auth.signInPassword(email.trim(), password)));
  };

  const sendLink = () =>
    run('magic', async () => {
      await auth.requestMagicLink(email.trim());
      rememberNext(next);
      setSent(true);
    });

  const passkey = () =>
    run('passkey', async () => {
      const opts = await auth.passkeyLoginStart(email.trim());
      const cred = await getCredential(opts);
      done(await auth.passkeyLoginFinish(cred));
    });

  if (sent) {
    return (
      <AuthCard>
        <h1>Check your email</h1>
        <p>
          If <strong>{email}</strong> has an Rconn account, we sent it a sign-in link. It works for 15 minutes, in this
          browser.
        </p>
        <p className="muted">Opening it on another device? The email also has a 6-digit code to enter there.</p>
        <button className="btn btn-quiet" onClick={() => setSent(false)}>
          Back
        </button>
      </AuthCard>
    );
  }

  if (!options) {
    return (
      <AuthCard>
        <h1>Sign in to the admin portal</h1>
        <form onSubmit={submitEmail} noValidate>
          <div className="field">
            <label htmlFor="email">Work email</label>
            <input
              id="email"
              type="email"
              autoComplete="username webauthn"
              autoFocus
              required
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
          </div>
          {error && (
            <div className="alert" role="alert">
              {error}
            </div>
          )}
          <button className="btn btn-block" disabled={busy !== null || !email.trim()}>
            {busy ? 'Checking…' : 'Continue'}
          </button>
        </form>
      </AuthCard>
    );
  }

  const showPasskey = options.passkey && passkeysSupported();
  return (
    <AuthCard>
      <h1>Sign in</h1>
      <p className="muted">
        {email}{' '}
        <button
          className="btn-link"
          onClick={() => {
            setOptions(null);
            setPassword('');
            setError(null);
          }}
        >
          Change
        </button>
      </p>
      {error && (
        <div className="alert" role="alert">
          {error}
        </div>
      )}
      {options.sso && (
        <a className="btn btn-block" href={auth.ssoStartUrl(options.sso.org_id)}>
          Continue with {options.sso.name} single sign-on
        </a>
      )}
      {options.sso && (options.password || options.magic_link || showPasskey) && <div className="or">or</div>}
      {options.password && (
        <form onSubmit={submitPassword}>
          <input type="email" autoComplete="username" value={email} readOnly hidden />
          <div className="field">
            <label htmlFor="password">Password</label>
            <input
              id="password"
              type="password"
              autoComplete="current-password"
              autoFocus={!options.sso}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </div>
          <button
            className={`btn btn-block ${options.sso ? 'btn-quiet' : ''}`}
            disabled={busy !== null || !password}
          >
            {busy === 'password' ? 'Signing in…' : 'Sign in'}
          </button>
        </form>
      )}
      {options.password && (showPasskey || options.magic_link) && <div className="or">or</div>}
      {showPasskey && (
        <button className="btn btn-quiet btn-block" onClick={passkey} disabled={busy !== null}>
          {busy === 'passkey' ? 'Waiting for your passkey…' : 'Sign in with a passkey'}
        </button>
      )}
      {options.magic_link && (
        <button className="btn btn-quiet btn-block" onClick={sendLink} disabled={busy !== null}>
          {busy === 'magic' ? 'Sending…' : 'Email me a sign-in link'}
        </button>
      )}
    </AuthCard>
  );
}
