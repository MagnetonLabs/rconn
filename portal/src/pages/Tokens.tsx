import { useMemo, useState } from 'react';
import * as api from '../api/computers';
import type { EnrollmentToken, NewEnrollmentToken } from '../api/computers';
import { listGroups } from '../api/policies';
import { useOrg, useCan } from '../lib/org';
import { useResource } from '../lib/useResource';
import { relTime, dateTime } from '../lib/format';
import { ConfirmDialog, CopyButton, Dialog, Empty, ErrorState, SkeletonRows, errorMessage, useToast } from '../components/ui';
import { ComputersTabs } from './Computers';
import { ApiError } from '../api/http';

const EXPIRY: [number, string][] = [
  [1, '1 hour'],
  [24, '1 day'],
  [168, '7 days'],
  [720, '30 days'],
];

export default function Tokens() {
  const org = useOrg();
  const canManage = useCan('computers.manage');
  const toast = useToast();
  const list = useResource(() => api.listTokens(org.org_id), [org.org_id]);
  const groups = useResource(() => listGroups(org.org_id), [org.org_id]);
  const groupName = useMemo(() => new Map((groups.data ?? []).map((g) => [g.id, g.name])), [groups.data]);

  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<NewEnrollmentToken | null>(null);
  const [deleting, setDeleting] = useState<EnrollmentToken | null>(null);

  const [label, setLabel] = useState('');
  const [group, setGroup] = useState('');
  const [hours, setHours] = useState(168);
  const [maxUses, setMaxUses] = useState('');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const openCreate = () => {
    setLabel('');
    setGroup('');
    setHours(168);
    setMaxUses('');
    setErr(null);
    setCreating(true);
  };

  const create = async () => {
    if (!label.trim()) {
      setErr('Enter a label.');
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const t = await api.createToken(org.org_id, {
        label: label.trim(),
        group_id: group || null,
        expires_in_hours: hours,
        max_uses: maxUses ? Number(maxUses) : null,
      });
      list.setData((xs) => [t, ...(xs ?? [])]);
      setCreating(false);
      setCreated(t);
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  if (!canManage) {
    return (
      <>
        <div className="page-head">
          <h1>Enrollment tokens</h1>
        </div>
        <ErrorState error={new ApiError(403, "forbidden", "")} />
      </>
    );
  }

  const now = Date.now();
  return (
    <>
      <div className="page-head">
        <h1>
          Computers
        </h1>
        <button className="btn" onClick={openCreate}>
          New enrollment token
        </button>
      </div>
      <ComputersTabs />
      <p className="muted" style={{ marginBottom: '0.6rem' }}>
        A token lets the Rconn installer join computers to this company. Give it a group so new computers get that
        group’s policy.
      </p>
      <div className="table-wrap">
        {list.error ? (
          <ErrorState error={list.error} onRetry={list.reload} />
        ) : (
          <table>
            <thead>
              <tr>
                <th scope="col">Label</th>
                <th scope="col">Group</th>
                <th scope="col">Uses</th>
                <th scope="col">Expires</th>
                <th scope="col" className="hide-sm">
                  Created by
                </th>
                <th scope="col">
                  <span className="visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            {list.loading && !list.data ? (
              <SkeletonRows cols={6} rows={3} />
            ) : (
              <tbody>
                {(list.data ?? []).map((t) => {
                  const expired = Date.parse(t.expires_at) < now;
                  const usedUp = t.max_uses !== null && t.uses >= t.max_uses;
                  return (
                    <tr key={t.id}>
                      <td>
                        <strong>{t.label}</strong>
                        {(expired || usedUp) && (
                          <span className="badge plain" style={{ marginLeft: '0.4rem' }}>
                            {expired ? 'Expired' : 'Used up'}
                          </span>
                        )}
                      </td>
                      <td>{t.group_id ? (groupName.get(t.group_id) ?? '—') : <span className="muted">No group</span>}</td>
                      <td className="nowrap">
                        {t.uses}
                        {t.max_uses !== null ? ` of ${t.max_uses}` : ''}
                      </td>
                      <td className="nowrap" title={dateTime(t.expires_at)}>
                        {relTime(t.expires_at)}
                      </td>
                      <td className="hide-sm">
                        {t.created_by} <span className="sub">{relTime(t.created_at)}</span>
                      </td>
                      <td className="actions">
                        <button className="btn-link danger" onClick={() => setDeleting(t)} aria-label={`Delete ${t.label}`}>
                          Delete
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            )}
          </table>
        )}
        {list.data?.length === 0 && (
          <Empty title="No enrollment tokens">
            <p>Create one to add computers to this company.</p>
          </Empty>
        )}
      </div>

      <Dialog
        open={creating}
        title="New enrollment token"
        onClose={() => setCreating(false)}
        onSubmit={create}
        actions={
          <>
            <button type="button" className="btn btn-quiet" onClick={() => setCreating(false)}>
              Cancel
            </button>
            <button type="submit" className="btn" disabled={busy}>
              {busy ? 'Creating…' : 'Create token'}
            </button>
          </>
        }
      >
        <div className="field">
          <label htmlFor="tk-label">Label</label>
          <input id="tk-label" autoFocus value={label} onChange={(e) => setLabel(e.target.value)} placeholder="Front desk rollout" />
        </div>
        <div className="field">
          <label htmlFor="tk-group">Group</label>
          <select id="tk-group" value={group} onChange={(e) => setGroup(e.target.value)}>
            <option value="">No group (company default policy)</option>
            {(groups.data ?? []).map((g) => (
              <option key={g.id} value={g.id}>
                {g.name}
              </option>
            ))}
          </select>
        </div>
        <div className="form-grid">
          <div className="field">
            <label htmlFor="tk-exp">Expires after</label>
            <select id="tk-exp" value={hours} onChange={(e) => setHours(Number(e.target.value))}>
              {EXPIRY.map(([h, l]) => (
                <option key={h} value={h}>
                  {l}
                </option>
              ))}
            </select>
          </div>
          <div className="field">
            <label htmlFor="tk-max">Max uses</label>
            <input
              id="tk-max"
              type="number"
              min={1}
              inputMode="numeric"
              placeholder="No limit"
              value={maxUses}
              onChange={(e) => setMaxUses(e.target.value.replace(/\D/g, ''))}
            />
          </div>
        </div>
        {err && (
          <div className="alert" role="alert">
            {err}
          </div>
        )}
      </Dialog>

      <Dialog
        open={created !== null}
        title="Token created"
        onClose={() => setCreated(null)}
        onSubmit={() => setCreated(null)}
        actions={
          <button type="submit" className="btn">
            Done
          </button>
        }
      >
        <p>
          Copy <strong>{created?.label}</strong> now. It won’t be shown again.
        </p>
        {created && (
          <div className="secret">
            <code data-testid="token-secret">{created.token}</code>
            <CopyButton text={created.token} />
          </div>
        )}
        <p className="muted">
          Put it in the deploy key file for the installer, or paste it into “Join your company” in the Rconn app.
        </p>
      </Dialog>

      <ConfirmDialog
        open={deleting !== null}
        title={`Delete ${deleting?.label ?? ''}?`}
        confirm="Delete token"
        body={<p>Installers using it stop working. Computers already enrolled stay enrolled.</p>}
        onClose={() => setDeleting(null)}
        onConfirm={async () => {
          if (!deleting) return;
          await api.deleteToken(org.org_id, deleting.id);
          list.setData((xs) => xs?.filter((x) => x.id !== deleting.id));
          toast('Token deleted');
        }}
      />
    </>
  );
}
