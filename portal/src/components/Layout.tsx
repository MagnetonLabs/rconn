import { useEffect, useRef, useState } from 'react';
import { Link, NavLink, Outlet, useLocation, useNavigate, useParams } from 'react-router';
import { useSession, rememberOrg } from '../lib/session';
import { OrgCtx } from '../lib/org';
import { can, ROLE_LABEL } from '../lib/roles';
import { initials } from './ui';
import type { Membership } from '../api/auth';

function OrgSwitcher({ current }: { current: Membership | null }) {
  const { me } = useSession();
  const nav = useNavigate();
  const loc = useLocation();
  if (!me || me.memberships.length === 0) return null;
  const section = loc.pathname.split('/')[3] ?? 'computers';
  return (
    <div className="org-switch">
      <label htmlFor="org-select" className="visually-hidden">
        Company
      </label>
      <select
        id="org-select"
        value={current?.org_id ?? ''}
        onChange={(e) => nav(`/o/${e.target.value}/${['audit', 'settings', 'people', 'groups', 'computers'].includes(section) ? section : 'computers'}`)}
      >
        {!current && <option value="">Choose a company</option>}
        {me.memberships.map((m) => (
          <option key={m.org_id} value={m.org_id}>
            {m.org_name}
          </option>
        ))}
      </select>
      {current && <span className="badge plain role-tag">{ROLE_LABEL[current.role]}</span>}
    </div>
  );
}

function AccountMenu() {
  const { me, signOut } = useSession();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const btn = useRef<HTMLButtonElement>(null);
  const nav = useNavigate();
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setOpen(false);
        btn.current?.focus();
      }
    };
    document.addEventListener('mousedown', onDoc);
    document.addEventListener('keydown', onKey);
    ref.current?.querySelector<HTMLElement>('.menu-item')?.focus();
    return () => {
      document.removeEventListener('mousedown', onDoc);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);
  if (!me) return null;
  const onMenuKey = (e: React.KeyboardEvent) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    e.preventDefault();
    const items = Array.from(ref.current?.querySelectorAll<HTMLElement>('.menu-item') ?? []);
    const i = items.indexOf(document.activeElement as HTMLElement);
    const n = e.key === 'ArrowDown' ? (i + 1) % items.length : (i - 1 + items.length) % items.length;
    items[n]?.focus();
  };
  return (
    <div className="menu" ref={ref}>
      <button
        ref={btn}
        className="menu-btn"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls="account-menu"
        onClick={() => setOpen((o) => !o)}
      >
        <span className="avatar" aria-hidden="true">
          {initials(me.user.name, me.user.email)}
        </span>
        <span className="menu-name">{me.user.name || me.user.email}</span>
        <span className="visually-hidden">Account menu</span>
      </button>
      {open && (
        <div className="menu-list" id="account-menu" role="menu" onKeyDown={onMenuKey}>
          <div className="who">
            <strong>{me.user.name}</strong>
            <span className="muted">{me.user.email}</span>
          </div>
          <Link role="menuitem" className="menu-item" to="/account" onClick={() => setOpen(false)}>
            Account and passkeys
          </Link>
          <button
            role="menuitem"
            className="menu-item"
            onClick={async () => {
              setOpen(false);
              await signOut();
              nav('/signin');
            }}
          >
            Sign out
          </button>
        </div>
      )}
    </div>
  );
}

export function Shell({ current, children }: { current: Membership | null; children: React.ReactNode }) {
  const role = current?.role;
  const base = current ? `/o/${current.org_id}` : null;
  return (
    <>
      <a className="skip" href="#main">
        Skip to content
      </a>
      <header className="app-head">
        <div className="bar">
          <Link className="brand" to="/" aria-label="Rconn admin home">
            <img src={`${import.meta.env.BASE_URL}favicon.svg`} alt="" width="22" height="22" />
            Rconn <small>Admin</small>
          </Link>
          <OrgSwitcher current={current} />
          <span className="spacer" />
          <AccountMenu />
        </div>
        {base && (
          <nav className="tabs" aria-label="Sections">
            <NavLink to={`${base}/computers`}>Computers</NavLink>
            <NavLink to={`${base}/people`}>People</NavLink>
            <NavLink to={`${base}/groups`}>Groups &amp; policies</NavLink>
            {can(role, 'audit.view') && <NavLink to={`${base}/audit`}>Audit</NavLink>}
            <NavLink to={`${base}/settings`}>Settings</NavLink>
          </nav>
        )}
      </header>
      <main id="main" tabIndex={-1}>
        {children}
      </main>
    </>
  );
}

/** `/o/:orgId/*`: a company the caller belongs to, else the same "not found" as the API's 404. */
export function OrgLayout() {
  const { orgId } = useParams();
  const { me } = useSession();
  const m = me?.memberships.find((x) => x.org_id === orgId) ?? null;
  useEffect(() => {
    if (m) rememberOrg(m.org_id);
  }, [m]);
  if (!m) {
    return (
      <Shell current={null}>
        <div className="state" role="alert">
          <h2>Company not found</h2>
          <p>It doesn’t exist, or you are not a member of it.</p>
          <Link className="btn btn-quiet" to="/">
            Go to your companies
          </Link>
        </div>
      </Shell>
    );
  }
  return (
    <OrgCtx.Provider value={m}>
      <Shell current={m}>
        <Outlet />
      </Shell>
    </OrgCtx.Provider>
  );
}
