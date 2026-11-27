import { useState, type FormEvent } from 'react';
import * as api from '../api/people';
import type { Person, Invite } from '../api/people';
import type { Role } from '../api/auth';
import { useOrg, useCan } from '../lib/org';
import { useSession } from '../lib/session';
import { useResource } from '../lib/useResource';
import { relTime, date, dateTime } from '../lib/format';
import { assignableRoles, canManageMember, ROLE_LABEL } from '../lib/roles';
import { ConfirmDialog, Empty, ErrorState, SkeletonRows, errorMessage, useToast } from '../components/ui';

const ROLE_HINT: Record<Role, string> = {
  owner: 'Everything, including SSO and the company itself',
  admin: 'Computers, people, policies and audit',
  technician: 'Connect to computers',
  viewer: 'See computers and policies',
};

export default function People() {
  const org = useOrg();
  const { me } = useSession();
  const canManage = useCan('people.manage');
  const toast = useToast();
  const people = useResource(() => api.listPeople(org.org_id), [org.org_id]);
  const invites = useResource(
    () => (canManage ? api.listInvites(org.org_id) : Promise.resolve([] as Invite[])),
    [org.org_id, canManage],
  );
  const ownerCount = (people.data ?? []).filter((p) => p.role === 'owner').length;
  const roles = assignableRoles(org.role);

  const [email, setEmail] = useState('');
  const [role, setRole] = useState<Role>('technician');
  const [sending, setSending] = useState(false);
  const [inviteErr, setInviteErr] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Person | null>(null);
  const [cancelling, setCancelling] = useState<Invite | null>(null);

  const send = async (e: FormEvent) => {
    e.preventDefault();
    if (!email.trim()) return;
    setSending(true);
    setInviteErr(null);
    try {
      const inv = await api.sendInvite(org.org_id, email.trim(), role);
      invites.setData((xs) => [...(xs ?? []), inv]);
      toast(`Invite sent to ${inv.email}`);
      setEmail('');
    } catch (err) {
      setInviteErr(errorMessage(err));
    } finally {
      setSending(false);
    }
  };

  const change = async (p: Person, r: Role) => {
    try {
      const updated = await api.changeRole(org.org_id, p.user_id, r);
      people.setData((xs) => xs?.map((x) => (x.user_id === p.user_id ? updated : x)));
      toast(`${p.name || p.email} is now ${ROLE_LABEL[r].toLowerCase()}`);
    } catch (e) {
      toast(errorMessage(e), 'error');
      people.reload();
    }
  };

  const sorted = [...(people.data ?? [])].sort(
    (a, b) => ['owner', 'admin', 'technician', 'viewer'].indexOf(a.role) - ['owner', 'admin', 'technician', 'viewer'].indexOf(b.role) || a.name.localeCompare(b.name),
  );

  return (
    <>
      <div className="page-head">
        <h1>
          People{people.data && <span className="count">{people.data.length}</span>}
        </h1>
      </div>

      {canManage && (
        <form className="card card-pad toolbar" onSubmit={send} aria-label="Invite a person" style={{ marginBottom: '0.75rem' }}>
          <strong>Invite</strong>
          <label className="visually-hidden" htmlFor="inv-email">
            Email
          </label>
          <input
            id="inv-email"
            type="email"
            placeholder="name@company.com"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            style={{ width: 'min(18rem, 100%)' }}
          />
          <label className="visually-hidden" htmlFor="inv-role">
            Role
          </label>
          <select id="inv-role" value={role} onChange={(e) => setRole(e.target.value as Role)}>
            {roles.map((r) => (
              <option key={r} value={r}>
                {ROLE_LABEL[r]}
              </option>
            ))}
          </select>
          <button className="btn" disabled={sending || !email.trim()}>
            {sending ? 'Sending…' : 'Send invite'}
          </button>
          <span className="muted hide-sm">{ROLE_HINT[role]}</span>
          {inviteErr && (
            <div className="alert" role="alert" style={{ flexBasis: '100%' }}>
              {inviteErr}
            </div>
          )}
        </form>
      )}

      <div className="table-wrap">
        {people.error ? (
          <ErrorState error={people.error} onRetry={people.reload} />
        ) : (
          <table>
            <thead>
              <tr>
                <th scope="col">Person</th>
                <th scope="col">Role</th>
                <th scope="col" className="hide-sm">
                  Joined
                </th>
                <th scope="col" className="hide-sm">
                  Last active
                </th>
                {canManage && (
                  <th scope="col">
                    <span className="visually-hidden">Actions</span>
                  </th>
                )}
              </tr>
            </thead>
            {people.loading && !people.data ? (
              <SkeletonRows cols={canManage ? 5 : 4} rows={4} />
            ) : (
              <tbody>
                {sorted.map((p) => {
                  const self = p.user_id === me?.user.id;
                  const manageable = canManageMember(org.role, p.role, { self, ownerCount });
                  return (
                    <tr key={p.user_id}>
                      <td>
                        <strong>{p.name || p.email}</strong>
                        {self && (
                          <span className="badge plain" style={{ marginLeft: '0.4rem' }}>
                            You
                          </span>
                        )}
                        <span className="sub">{p.email}</span>
                      </td>
                      <td>
                        {manageable ? (
                          <>
                            <label className="visually-hidden" htmlFor={`role-${p.user_id}`}>
                              Role for {p.name || p.email}
                            </label>
                            <select
                              id={`role-${p.user_id}`}
                              value={p.role}
                              onChange={(e) => change(p, e.target.value as Role)}
                            >
                              {roles.map((r) => (
                                <option key={r} value={r}>
                                  {ROLE_LABEL[r]}
                                </option>
                              ))}
                            </select>
                          </>
                        ) : (
                          ROLE_LABEL[p.role]
                        )}
                      </td>
                      <td className="hide-sm nowrap" title={dateTime(p.joined_at)}>
                        {date(p.joined_at)}
                      </td>
                      <td className="hide-sm nowrap" title={dateTime(p.last_active)}>
                        {relTime(p.last_active)}
                      </td>
                      {canManage && (
                        <td className="actions">
                          {manageable && (
                            <button
                              className="btn-link danger"
                              onClick={() => setRemoving(p)}
                              aria-label={`Remove ${p.name || p.email}`}
                            >
                              Remove
                            </button>
                          )}
                        </td>
                      )}
                    </tr>
                  );
                })}
              </tbody>
            )}
          </table>
        )}
      </div>

      {canManage && (
        <section className="section" aria-labelledby="inv-h">
          <h2 id="inv-h">
            Pending invites{invites.data && <span className="count">{invites.data.length}</span>}
          </h2>
          <div className="table-wrap">
            {invites.error ? (
              <ErrorState error={invites.error} onRetry={invites.reload} />
            ) : invites.data?.length === 0 ? (
              <Empty title="No pending invites" />
            ) : (
              <table>
                <thead>
                  <tr>
                    <th scope="col">Email</th>
                    <th scope="col">Role</th>
                    <th scope="col" className="hide-sm">
                      Invited by
                    </th>
                    <th scope="col">Expires</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                {invites.loading && !invites.data ? (
                  <SkeletonRows cols={5} rows={2} />
                ) : (
                  <tbody>
                    {(invites.data ?? []).map((i) => (
                      <tr key={i.id}>
                        <td>{i.email}</td>
                        <td>{ROLE_LABEL[i.role]}</td>
                        <td className="hide-sm">{i.invited_by}</td>
                        <td className="nowrap" title={dateTime(i.expires_at)}>
                          {relTime(i.expires_at)}
                        </td>
                        <td className="actions">
                          <button className="btn-link danger" onClick={() => setCancelling(i)} aria-label={`Cancel invite for ${i.email}`}>
                            Cancel
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                )}
              </table>
            )}
          </div>
        </section>
      )}

      <ConfirmDialog
        open={removing !== null}
        title={`Remove ${removing?.name || removing?.email || ''}?`}
        confirm="Remove"
        body={<p>They lose access to this company now, and any session they have open ends.</p>}
        onClose={() => setRemoving(null)}
        onConfirm={async () => {
          if (!removing) return;
          await api.removePerson(org.org_id, removing.user_id);
          people.setData((xs) => xs?.filter((x) => x.user_id !== removing.user_id));
          toast(`${removing.name || removing.email} removed`);
        }}
      />
      <ConfirmDialog
        open={cancelling !== null}
        title={`Cancel the invite for ${cancelling?.email ?? ''}?`}
        confirm="Cancel invite"
        body={<p>The link in their email stops working.</p>}
        onClose={() => setCancelling(null)}
        onConfirm={async () => {
          if (!cancelling) return;
          await api.cancelInvite(org.org_id, cancelling.id);
          invites.setData((xs) => xs?.filter((x) => x.id !== cancelling.id));
          toast('Invite cancelled');
        }}
      />
    </>
  );
}
