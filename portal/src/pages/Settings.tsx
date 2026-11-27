import { useEffect, useState, type FormEvent } from 'react';
import * as orgApi from '../api/org';
import * as ssoApi from '../api/sso';
import type { SsoConfig, SsoDomain } from '../api/sso';
import type { Role } from '../api/auth';
import { useOrg, useCan } from '../lib/org';
import { useSession } from '../lib/session';
import { useResource } from '../lib/useResource';
import { date } from '../lib/format';
import { ROLE_LABEL } from '../lib/roles';
import { CopyButton, ErrorState, Loading, Switch, errorMessage, useToast } from '../components/ui';

export default function Settings() {
  const org = useOrg();
  const canRename = useCan('org.rename');
  const canSso = useCan('sso.manage');
  return (
    <>
      <div className="page-head">
        <h1>Settings</h1>
      </div>
      <div className="settings-grid">
        <CompanyCard orgId={org.org_id} canRename={canRename} />
        <section className="card card-pad" aria-labelledby="sso-h">
          <h2 id="sso-h" style={{ marginBottom: '0.5rem' }}>
            Single sign-on
          </h2>
          {canSso ? <SsoCard orgId={org.org_id} /> : <p className="muted">Only owners can see and change single sign-on.</p>}
        </section>
      </div>
    </>
  );
}

function CompanyCard({ orgId, canRename }: { orgId: string; canRename: boolean }) {
  const toast = useToast();
  const { refresh } = useSession();
  const res = useResource(() => orgApi.getOrg(orgId), [orgId]);
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    if (res.data) setName(res.data.name);
  }, [res.data]);

  const save = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      const o = await orgApi.renameOrg(orgId, name.trim());
      res.setData(() => o);
      await refresh();
      toast('Company renamed');
    } catch (e2) {
      setErr(errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card card-pad" aria-labelledby="co-h">
      <h2 id="co-h" style={{ marginBottom: '0.5rem' }}>
        Company
      </h2>
      {res.error ? (
        <ErrorState error={res.error} onRetry={res.reload} />
      ) : !res.data ? (
        <Loading />
      ) : (
        <form onSubmit={save}>
          <div className="field">
            <label htmlFor="co-name">Name</label>
            <input id="co-name" value={name} onChange={(e) => setName(e.target.value)} disabled={!canRename} />
          </div>
          <dl className="kv" style={{ marginTop: '0.75rem' }}>
            <dt>Plan</dt>
            <dd style={{ textTransform: 'capitalize' }}>{res.data.plan}</dd>
            <dt>Created</dt>
            <dd>{date(res.data.created_at)}</dd>
            <dt>Company ID</dt>
            <dd className="mono">{res.data.id}</dd>
          </dl>
          {err && (
            <div className="alert" role="alert" style={{ marginTop: '0.6rem' }}>
              {err}
            </div>
          )}
          {canRename ? (
            <div className="form-actions">
              <button className="btn" disabled={busy || !name.trim() || name.trim() === res.data.name}>
                {busy ? 'Saving…' : 'Save'}
              </button>
            </div>
          ) : (
            <p className="muted" style={{ marginTop: '0.6rem' }}>
              Only owners can rename the company.
            </p>
          )}
        </form>
      )}
    </section>
  );
}

function SsoCard({ orgId }: { orgId: string }) {
  const toast = useToast();
  const res = useResource(() => ssoApi.getSso(orgId), [orgId]);
  const [form, setForm] = useState<{ issuer: string; client_id: string; client_secret: string; auto_join_role: Role; enabled: boolean } | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [newDomain, setNewDomain] = useState('');
  const [domainErr, setDomainErr] = useState<string | null>(null);
  const [verifying, setVerifying] = useState<string | null>(null);

  useEffect(() => {
    if (res.data && !form)
      setForm({
        issuer: res.data.issuer,
        client_id: res.data.client_id,
        client_secret: '',
        auto_join_role: res.data.auto_join_role,
        enabled: res.data.enabled,
      });
  }, [res.data, form]);

  if (res.error) return <ErrorState error={res.error} onRetry={res.reload} />;
  if (!res.data || !form) return <Loading />;
  const cfg: SsoConfig = res.data;
  const configured = Boolean(cfg.issuer && cfg.client_id);

  const save = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      const out = await ssoApi.putSso(orgId, { ...form, client_secret: form.client_secret || undefined });
      res.setData(() => out);
      setForm({ ...form, client_secret: '', enabled: out.enabled });
      toast('Single sign-on saved');
    } catch (e2) {
      setErr(errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  const setDomain = (d: SsoDomain) =>
    res.setData((c) => c && { ...c, domains: c.domains.some((x) => x.domain === d.domain) ? c.domains.map((x) => (x.domain === d.domain ? d : x)) : [...c.domains, d] });

  const add = async (e: FormEvent) => {
    e.preventDefault();
    setDomainErr(null);
    try {
      setDomain(await ssoApi.addDomain(orgId, newDomain.trim().toLowerCase()));
      setNewDomain('');
    } catch (e2) {
      setDomainErr(errorMessage(e2));
    }
  };

  const verify = async (domain: string) => {
    setVerifying(domain);
    setDomainErr(null);
    try {
      const d = await ssoApi.verifyDomain(orgId, domain);
      setDomain(d);
      if (d.verified) toast(`${domain} verified`);
      else setDomainErr(`The TXT record for ${domain} wasn’t found yet. DNS changes can take a few minutes.`);
    } catch (e2) {
      setDomainErr(errorMessage(e2));
    } finally {
      setVerifying(null);
    }
  };

  return (
    <>
      <form onSubmit={save}>
        <div className="policy-row" style={{ paddingTop: 0 }}>
          <label className="txt" htmlFor="sso-on">
            Sign in with your identity provider
            <span>{cfg.enabled ? 'On. People at verified domains see the SSO button.' : 'Off'}</span>
          </label>
          <Switch id="sso-on" label="Single sign-on enabled" checked={form.enabled} onChange={(v) => setForm({ ...form, enabled: v })} />
        </div>
        <div className="form-grid" style={{ marginTop: '0.6rem' }}>
          <div className="field" style={{ gridColumn: '1 / -1' }}>
            <label htmlFor="sso-issuer">Issuer URL</label>
            <input
              id="sso-issuer"
              type="text"
              inputMode="url"
              placeholder="https://login.example.com"
              value={form.issuer}
              onChange={(e) => setForm({ ...form, issuer: e.target.value })}
            />
          </div>
          <div className="field">
            <label htmlFor="sso-client">Client ID</label>
            <input id="sso-client" value={form.client_id} onChange={(e) => setForm({ ...form, client_id: e.target.value })} />
          </div>
          <div className="field">
            <label htmlFor="sso-secret">Client secret</label>
            <input
              id="sso-secret"
              type="password"
              autoComplete="new-password"
              placeholder={configured ? 'Saved. Type to replace' : ''}
              value={form.client_secret}
              onChange={(e) => setForm({ ...form, client_secret: e.target.value })}
            />
          </div>
          <div className="field">
            <label htmlFor="sso-role">New people join as</label>
            <select id="sso-role" value={form.auto_join_role} onChange={(e) => setForm({ ...form, auto_join_role: e.target.value as Role })}>
              {(['admin', 'technician', 'viewer'] as Role[]).map((r) => (
                <option key={r} value={r}>
                  {ROLE_LABEL[r]}
                </option>
              ))}
            </select>
          </div>
        </div>
        <p className="muted" style={{ marginTop: '0.5rem' }}>
          Redirect URI: <code>{`${location.origin}/api/v1/auth/sso/callback`}</code>
        </p>
        {err && (
          <div className="alert" role="alert" style={{ marginTop: '0.6rem' }}>
            {err}
          </div>
        )}
        <div className="form-actions">
          <button className="btn" disabled={busy}>
            {busy ? 'Saving…' : 'Save'}
          </button>
        </div>
      </form>

      <h3 style={{ margin: '1rem 0 0.4rem' }}>Domains</h3>
      <p className="muted" style={{ marginBottom: '0.5rem' }}>
        People with an address at a verified domain can sign in with SSO and join automatically.
      </p>
      {cfg.domains.length > 0 && (
        <div className="table-wrap" style={{ marginBottom: '0.5rem' }}>
          <table>
            <thead>
              <tr>
                <th scope="col">Domain</th>
                <th scope="col">Status</th>
                <th scope="col">
                  <span className="visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {cfg.domains.map((d) => (
                <tr key={d.domain}>
                  <td>
                    <strong>{d.domain}</strong>
                    {!d.verified && (
                      <span className="sub">
                        Add a TXT record at <code>{d.domain}</code>: <code>{d.txt_record}</code>
                      </span>
                    )}
                  </td>
                  <td>{d.verified ? <span className="badge ok">Verified</span> : <span className="badge warn">Not verified</span>}</td>
                  <td className="actions">
                    {!d.verified && (
                      <>
                        <CopyButton text={d.txt_record} label="Copy TXT" />
                        <button className="btn btn-quiet btn-sm" onClick={() => verify(d.domain)} disabled={verifying === d.domain}>
                          {verifying === d.domain ? 'Checking…' : 'Verify'}
                        </button>
                      </>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <form onSubmit={add} className="toolbar">
        <label className="visually-hidden" htmlFor="sso-domain">
          Domain
        </label>
        <input id="sso-domain" placeholder="example.com" value={newDomain} onChange={(e) => setNewDomain(e.target.value)} />
        <button className="btn btn-quiet" disabled={!newDomain.trim()}>
          Add domain
        </button>
      </form>
      {domainErr && (
        <div className="alert" role="alert">
          {domainErr}
        </div>
      )}
    </>
  );
}
