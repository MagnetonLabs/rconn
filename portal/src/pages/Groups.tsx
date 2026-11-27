import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useSearchParams } from 'react-router';
import * as api from '../api/policies';
import type { Group, Policy, Day } from '../api/policies';
import { DAYS } from '../api/policies';
import { useOrg, useCan } from '../lib/org';
import { useResource } from '../lib/useResource';
import { ConfirmDialog, Dialog, ErrorState, Loading, Switch, errorMessage, useToast } from '../components/ui';

const TOGGLES: { key: keyof Omit<Policy, 'allowed_hours'>; title: string; text: string }[] = [
  { key: 'clipboard', title: 'Clipboard', text: 'Copy and paste between the two computers' },
  { key: 'file_transfer', title: 'File transfer', text: 'Send files in both directions' },
  { key: 'audio', title: 'Sound', text: 'Hear what plays on the remote computer' },
  { key: 'require_approval', title: 'Ask before connecting', text: 'Someone at the computer accepts each session' },
  { key: 'show_indicator', title: 'Show that a session is on', text: 'The computer shows when someone is connected' },
  {
    key: 'allow_access_password',
    title: 'Allow ID and access password',
    text: 'People outside the company can connect with the computer’s password',
  },
];

const DAY_LABEL: Record<Day, string> = { mon: 'Mon', tue: 'Tue', wed: 'Wed', thu: 'Thu', fri: 'Fri', sat: 'Sat', sun: 'Sun' };

function timeZones(): string[] {
  try {
    return Intl.supportedValuesOf('timeZone');
  } catch {
    return ['UTC', 'America/New_York', 'America/Chicago', 'America/Denver', 'America/Los_Angeles', 'Europe/London', 'Europe/Berlin'];
  }
}

const localZone = () => Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';

export default function Groups() {
  const org = useOrg();
  const canManage = useCan('groups.manage');
  const toast = useToast();
  const [params, setParams] = useSearchParams();
  const selected = params.get('g') ?? 'default';
  const groups = useResource(() => api.listGroups(org.org_id), [org.org_id]);
  const current = groups.data?.find((g) => g.id === selected) ?? null;
  const groupId = selected === 'default' ? null : selected;

  const [newName, setNewName] = useState('');
  const [renaming, setRenaming] = useState<Group | null>(null);
  const [renameTo, setRenameTo] = useState('');
  const [renameErr, setRenameErr] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<Group | null>(null);
  const [createErr, setCreateErr] = useState<string | null>(null);

  const select = (id: string) => setParams(id === 'default' ? {} : { g: id }, { replace: true });

  const create = async (e: FormEvent) => {
    e.preventDefault();
    if (!newName.trim()) return;
    setCreateErr(null);
    try {
      const g = await api.createGroup(org.org_id, newName.trim());
      groups.setData((xs) => [...(xs ?? []), g]);
      setNewName('');
      select(g.id);
      toast(`Group ${g.name} created`);
    } catch (err) {
      setCreateErr(errorMessage(err));
    }
  };

  const rename = async () => {
    if (!renaming) return;
    setRenameErr(null);
    try {
      const g = await api.renameGroup(org.org_id, renaming.id, renameTo.trim());
      groups.setData((xs) => xs?.map((x) => (x.id === g.id ? g : x)));
      setRenaming(null);
      toast('Group renamed');
    } catch (err) {
      setRenameErr(errorMessage(err));
    }
  };

  return (
    <>
      <div className="page-head">
        <h1>Groups &amp; policies</h1>
      </div>
      <div className="split">
        <aside className="card" aria-label="Groups">
          {groups.error ? (
            <ErrorState error={groups.error} onRetry={groups.reload} />
          ) : (
            <ul className="group-list">
              <li>
                <button className="pick" aria-current={selected === 'default'} onClick={() => select('default')}>
                  <span>Company default</span>
                  <span className="n">ungrouped</span>
                </button>
              </li>
              <li className="sep" role="separator" />
              {groups.loading && !groups.data && <li className="muted" style={{ padding: '0.4rem 0.55rem' }}>Loading…</li>}
              {groups.data?.length === 0 && (
                <li className="muted" style={{ padding: '0.4rem 0.55rem' }}>
                  No groups yet
                </li>
              )}
              {groups.data?.map((g) => (
                <li key={g.id}>
                  <button className="pick" aria-current={selected === g.id} onClick={() => select(g.id)}>
                    <span>{g.name}</span>
                    <span className="n" aria-label={`${g.computers} computers`}>
                      {g.computers}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
          {canManage && (
            <form onSubmit={create} style={{ display: 'flex', gap: '0.4rem', padding: '0.5rem', borderTop: '1px solid var(--rule)', flexWrap: 'wrap' }}>
              <label className="visually-hidden" htmlFor="new-group">
                New group name
              </label>
              <input
                id="new-group"
                placeholder="New group"
                value={newName}
                onChange={(e) => setNewName(e.target.value)}
                style={{ flex: 1 }}
              />
              <button className="btn btn-sm" disabled={!newName.trim()}>
                Add
              </button>
              {createErr && (
                <div className="alert" role="alert" style={{ flexBasis: '100%' }}>
                  {createErr}
                </div>
              )}
            </form>
          )}
        </aside>

        <section className="card card-pad" aria-labelledby="policy-h">
          {selected !== 'default' && groups.data && !current ? (
            <ErrorState error={{ message: 'This group was deleted.' }} />
          ) : (
            <>
              <div className="page-head" style={{ marginBottom: '0.25rem' }}>
                <h2 id="policy-h">{groupId ? (current?.name ?? '…') : 'Company default'}</h2>
                {canManage && current && (
                  <>
                    <button
                      className="btn-link"
                      onClick={() => {
                        setRenameTo(current.name);
                        setRenameErr(null);
                        setRenaming(current);
                      }}
                    >
                      Rename
                    </button>
                    <button className="btn-link danger" onClick={() => setDeleting(current)}>
                      Delete group
                    </button>
                  </>
                )}
              </div>
              <p className="muted" style={{ marginBottom: '0.5rem' }}>
                {groupId
                  ? `Applies to the ${current?.computers ?? 0} computers in this group.`
                  : 'Applies to computers that aren’t in a group.'}
              </p>
              <PolicyEditor key={`${org.org_id}:${selected}`} orgId={org.org_id} groupId={groupId} readOnly={!canManage} />
            </>
          )}
        </section>
      </div>

      <Dialog
        open={renaming !== null}
        title="Rename group"
        onClose={() => setRenaming(null)}
        onSubmit={rename}
        actions={
          <>
            <button type="button" className="btn btn-quiet" onClick={() => setRenaming(null)}>
              Cancel
            </button>
            <button type="submit" className="btn" disabled={!renameTo.trim()}>
              Save
            </button>
          </>
        }
      >
        <div className="field">
          <label htmlFor="rename-group">Name</label>
          <input id="rename-group" autoFocus value={renameTo} onChange={(e) => setRenameTo(e.target.value)} />
        </div>
        {renameErr && <div className="alert">{renameErr}</div>}
      </Dialog>

      <ConfirmDialog
        open={deleting !== null}
        title={`Delete ${deleting?.name ?? ''}?`}
        confirm="Delete group"
        body={<p>Its {deleting?.computers ?? 0} computers move out of the group and get the company default policy.</p>}
        onClose={() => setDeleting(null)}
        onConfirm={async () => {
          if (!deleting) return;
          await api.deleteGroup(org.org_id, deleting.id);
          groups.setData((xs) => xs?.filter((x) => x.id !== deleting.id));
          select('default');
          toast('Group deleted');
        }}
      />
    </>
  );
}

function PolicyEditor({ orgId, groupId, readOnly }: { orgId: string; groupId: string | null; readOnly: boolean }) {
  const toast = useToast();
  const res = useResource(() => api.getPolicy(orgId, groupId), [orgId, groupId]);
  const [draft, setDraft] = useState<Policy | null>(null);
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const zones = useMemo(timeZones, []);
  useEffect(() => {
    if (res.data) setDraft(res.data);
  }, [res.data]);

  if (res.error) return <ErrorState error={res.error} onRetry={res.reload} />;
  if (!draft || !res.data) return <Loading />;

  const dirty = JSON.stringify(draft) !== JSON.stringify(res.data);
  const set = <K extends keyof Policy>(k: K, v: Policy[K]) => setDraft({ ...draft, [k]: v });
  const hours = draft.allowed_hours;

  const save = async (e: FormEvent) => {
    e.preventDefault();
    setSaving(true);
    setErr(null);
    try {
      const p = await api.putPolicy(orgId, groupId, draft);
      res.setData(() => p);
      setDraft(p);
      toast('Policy saved');
    } catch (e2) {
      setErr(errorMessage(e2));
    } finally {
      setSaving(false);
    }
  };

  return (
    <form onSubmit={save} aria-label="Policy">
      <fieldset disabled={readOnly} style={{ border: 0, padding: 0, margin: 0 }}>
        <legend className="visually-hidden">Session permissions</legend>
        <div className="policy-grid">
          {TOGGLES.map((t) => (
            <div className="policy-row" key={t.key}>
              <label className="txt" htmlFor={`pol-${t.key}`}>
                {t.title}
                <span>{t.text}</span>
              </label>
              <Switch id={`pol-${t.key}`} label={t.title} checked={draft[t.key]} onChange={(v) => set(t.key, v)} disabled={readOnly} />
            </div>
          ))}
          <div className="policy-row">
            <label className="txt" htmlFor="pol-hours">
              Only during set hours
              <span>Outside them, sessions are refused</span>
            </label>
            <Switch
              id="pol-hours"
              label="Only during set hours"
              checked={hours !== null}
              disabled={readOnly}
              onChange={(v) =>
                set(
                  'allowed_hours',
                  v ? { timezone: localZone(), days: ['mon', 'tue', 'wed', 'thu', 'fri'], start: '08:00', end: '18:00' } : null,
                )
              }
            />
          </div>
        </div>
        {hours && (
          <div className="form-grid" style={{ marginTop: '0.75rem' }}>
            <div className="field" style={{ gridColumn: '1 / -1' }}>
              <span id="days-l" style={{ fontWeight: 600 }}>
                Days
              </span>
              <div className="days" role="group" aria-labelledby="days-l">
                {DAYS.map((d) => (
                  <label key={d}>
                    <input
                      type="checkbox"
                      checked={hours.days.includes(d)}
                      onChange={(e) =>
                        set('allowed_hours', {
                          ...hours,
                          days: e.target.checked ? DAYS.filter((x) => x === d || hours.days.includes(x)) : hours.days.filter((x) => x !== d),
                        })
                      }
                    />
                    {DAY_LABEL[d]}
                  </label>
                ))}
              </div>
            </div>
            <div className="field">
              <label htmlFor="pol-start">From</label>
              <input id="pol-start" type="time" value={hours.start} onChange={(e) => set('allowed_hours', { ...hours, start: e.target.value })} />
            </div>
            <div className="field">
              <label htmlFor="pol-end">Until</label>
              <input id="pol-end" type="time" value={hours.end} onChange={(e) => set('allowed_hours', { ...hours, end: e.target.value })} />
            </div>
            <div className="field">
              <label htmlFor="pol-tz">Time zone</label>
              <select id="pol-tz" value={hours.timezone} onChange={(e) => set('allowed_hours', { ...hours, timezone: e.target.value })}>
                {!zones.includes(hours.timezone) && <option value={hours.timezone}>{hours.timezone}</option>}
                {zones.map((z) => (
                  <option key={z} value={z}>
                    {z.replace(/_/g, ' ')}
                  </option>
                ))}
              </select>
            </div>
            {hours.end <= hours.start && hours.end !== hours.start && (
              <p className="muted" style={{ gridColumn: '1 / -1' }}>
                Ends the next day (the window crosses midnight).
              </p>
            )}
          </div>
        )}
      </fieldset>
      {err && (
        <div className="alert" role="alert" style={{ marginTop: '0.75rem' }}>
          {err}
        </div>
      )}
      {!readOnly ? (
        <div className="form-actions">
          <button className="btn" disabled={!dirty || saving}>
            {saving ? 'Saving…' : 'Save policy'}
          </button>
          <button type="button" className="btn btn-quiet" disabled={!dirty || saving} onClick={() => setDraft(res.data!)}>
            Discard changes
          </button>
          {dirty && <span className="muted">Unsaved changes</span>}
        </div>
      ) : (
        <p className="muted" style={{ marginTop: '0.75rem' }}>
          Only owners and admins can change policies.
        </p>
      )}
    </form>
  );
}
