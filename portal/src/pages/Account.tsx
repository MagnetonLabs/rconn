import { useEffect, useState, type FormEvent } from 'react';
import * as auth from '../api/auth';
import type { Passkey } from '../api/auth';
import { useSession } from '../lib/session';
import { useResource } from '../lib/useResource';
import { relTime, date } from '../lib/format';
import { passkeysSupported, createCredential } from '../lib/webauthn';
import { ConfirmDialog, Empty, ErrorState, SkeletonRows, errorMessage, useToast } from '../components/ui';

export default function Account() {
  const { me, setMe } = useSession();
  const toast = useToast();
  const [name, setName] = useState(me?.user.name ?? '');
  const [busy, setBusy] = useState(false);
  const keys = useResource(() => auth.listPasskeys(), []);
  const [pkName, setPkName] = useState('');
  const [adding, setAdding] = useState(false);
  const [pkErr, setPkErr] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Passkey | null>(null);
  const supported = passkeysSupported();

  useEffect(() => {
    if (me) setName(me.user.name);
  }, [me]);
  if (!me) return null;

  const saveName = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    try {
      setMe(await auth.updateMe(name.trim()));
      toast('Name saved');
    } catch (e2) {
      toast(errorMessage(e2), 'error');
    } finally {
      setBusy(false);
    }
  };

  const addKey = async (e: FormEvent) => {
    e.preventDefault();
    const label = pkName.trim() || 'Passkey';
    setAdding(true);
    setPkErr(null);
    try {
      const opts = await auth.passkeyRegisterStart(label);
      const cred = await createCredential(opts);
      const pk = await auth.passkeyRegisterFinish(label, cred);
      keys.setData((xs) => [...(xs ?? []), pk]);
      setPkName('');
      toast('Passkey added');
    } catch (e2) {
      setPkErr(e2 instanceof DOMException && e2.name === 'NotAllowedError' ? 'Cancelled, or the passkey prompt timed out.' : errorMessage(e2));
    } finally {
      setAdding(false);
    }
  };

  return (
    <>
      <div className="page-head">
        <h1>Account</h1>
      </div>
      <div className="settings-grid">
        <section className="card card-pad" aria-labelledby="acct-h">
          <h2 id="acct-h" style={{ marginBottom: '0.5rem' }}>
            You
          </h2>
          <form onSubmit={saveName}>
            <div className="field">
              <label htmlFor="acct-name">Name</label>
              <input id="acct-name" value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <dl className="kv" style={{ marginTop: '0.75rem' }}>
              <dt>Email</dt>
              <dd>{me.user.email}</dd>
              <dt>Companies</dt>
              <dd>{me.memberships.map((m) => m.org_name).join(', ') || 'None'}</dd>
            </dl>
            <div className="form-actions">
              <button className="btn" disabled={busy || !name.trim() || name.trim() === me.user.name}>
                Save
              </button>
            </div>
          </form>
        </section>

        <section className="card card-pad" aria-labelledby="pk-h">
          <h2 id="pk-h" style={{ marginBottom: '0.25rem' }}>
            Passkeys
          </h2>
          <p className="muted" style={{ marginBottom: '0.6rem' }}>
            Sign in with Touch ID, Windows Hello or a security key instead of a password.
          </p>
          <div className="table-wrap" style={{ marginBottom: '0.6rem' }}>
            {keys.error ? (
              <ErrorState error={keys.error} onRetry={keys.reload} />
            ) : (
              <table>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col">Added</th>
                    <th scope="col">Last used</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                {keys.loading && !keys.data ? (
                  <SkeletonRows cols={4} rows={2} />
                ) : (
                  <tbody>
                    {(keys.data ?? []).map((k) => (
                      <tr key={k.id}>
                        <td>
                          <strong>{k.name}</strong>
                        </td>
                        <td className="nowrap">{date(k.created_at)}</td>
                        <td className="nowrap">{relTime(k.last_used)}</td>
                        <td className="actions">
                          <button className="btn-link danger" onClick={() => setRemoving(k)} aria-label={`Remove ${k.name}`}>
                            Remove
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                )}
              </table>
            )}
            {keys.data?.length === 0 && <Empty title="No passkeys yet" />}
          </div>
          {supported ? (
            <form className="toolbar" onSubmit={addKey}>
              <label className="visually-hidden" htmlFor="pk-name">
                Passkey name
              </label>
              <input id="pk-name" placeholder="MacBook Touch ID" value={pkName} onChange={(e) => setPkName(e.target.value)} />
              <button className="btn" disabled={adding}>
                {adding ? 'Waiting for your device…' : 'Add passkey'}
              </button>
            </form>
          ) : (
            <p className="muted">Passkeys need a secure (https) connection and a browser that supports them.</p>
          )}
          {pkErr && (
            <div className="alert" role="alert">
              {pkErr}
            </div>
          )}
        </section>
      </div>
      <ConfirmDialog
        open={removing !== null}
        title={`Remove ${removing?.name ?? ''}?`}
        confirm="Remove passkey"
        body={<p>You can’t sign in with it any more. Your other sign-in methods keep working.</p>}
        onClose={() => setRemoving(null)}
        onConfirm={async () => {
          if (!removing) return;
          await auth.deletePasskey(removing.id);
          keys.setData((xs) => xs?.filter((x) => x.id !== removing.id));
          toast('Passkey removed');
        }}
      />
    </>
  );
}
