import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { NavLink } from 'react-router';
import * as api from '../api/computers';
import type { Computer, ComputerStatus } from '../api/computers';
import { listGroups, type Group } from '../api/policies';
import { useOrg, useCan } from '../lib/org';
import { useResource } from '../lib/useResource';
import { relTime, dateTime, plural } from '../lib/format';
import { ConfirmDialog, Empty, ErrorState, SkeletonRows, errorMessage, useToast } from '../components/ui';

export function ComputersTabs() {
  const org = useOrg();
  const canManage = useCan('computers.manage');
  if (!canManage) return null;
  return (
    <nav className="subtabs" aria-label="Computers">
      <NavLink end to={`/o/${org.org_id}/computers`}>
        Computers
      </NavLink>
      <NavLink to={`/o/${org.org_id}/computers/tokens`}>Enrollment tokens</NavLink>
    </nav>
  );
}

function useDebounced<T>(v: T, ms: number): T {
  const [d, setD] = useState(v);
  useEffect(() => {
    const t = setTimeout(() => setD(v), ms);
    return () => clearTimeout(t);
  }, [v, ms]);
  return d;
}

const UNGROUPED = '__none';

export default function Computers() {
  const org = useOrg();
  const canManage = useCan('computers.manage');
  const toast = useToast();
  const [q, setQ] = useState('');
  const [group, setGroup] = useState('');
  const [status, setStatus] = useState<'' | ComputerStatus>('');
  const dq = useDebounced(q.trim(), 200);

  const groups = useResource(() => listGroups(org.org_id), [org.org_id]);
  const list = useResource(
    () =>
      api.listComputers(org.org_id, {
        q: dq || undefined,
        group_id: group && group !== UNGROUPED ? group : undefined,
        status: status || undefined,
      }),
    [org.org_id, dq, group, status],
  );
  const rows = useMemo(
    () => (list.data ?? []).filter((c) => group !== UNGROUPED || c.group_id === null),
    [list.data, group],
  );
  const groupName = useMemo(() => new Map((groups.data ?? []).map((g) => [g.id, g.name])), [groups.data]);

  const [editing, setEditing] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<Computer | null>(null);

  const replace = (c: Computer) => list.setData((xs) => xs?.map((x) => (x.id === c.id ? c : x)));

  const rename = async (c: Computer, name: string) => {
    const n = name.trim();
    if (!n || n === c.name) {
      setEditing(null);
      return;
    }
    try {
      replace(await api.updateComputer(org.org_id, c.id, { name: n }));
      setEditing(null);
      toast(`Renamed to ${n}`);
    } catch (e) {
      toast(errorMessage(e), 'error');
    }
  };

  const move = async (c: Computer, groupId: string) => {
    try {
      replace(await api.updateComputer(org.org_id, c.id, { group_id: groupId || null }));
      toast(`${c.name} moved to ${groupId ? groupName.get(groupId) : 'no group'}`);
      groups.reload();
    } catch (e) {
      toast(errorMessage(e), 'error');
    }
  };

  const all = list.data ?? [];
  const online = all.filter((c) => c.online && !c.revoked_at).length;
  const outdated = all.filter((c) => c.needs_update && !c.revoked_at).length;
  const filtered = Boolean(dq || group || status);

  return (
    <>
      <div className="page-head">
        <h1>
          Computers{list.data && <span className="count">{rows.length}</span>}
        </h1>
        {list.data && (
          <span className="muted">
            {online} online{outdated > 0 && <> · {plural(outdated, 'needs', 'need')} update</>}
          </span>
        )}
      </div>
      <ComputersTabs />
      <div className="toolbar" role="search">
        <label className="visually-hidden" htmlFor="pc-q">
          Search computers
        </label>
        <input
          id="pc-q"
          type="search"
          placeholder="Search name, hostname or ID"
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
        <label className="visually-hidden" htmlFor="pc-group">
          Group
        </label>
        <select id="pc-group" value={group} onChange={(e) => setGroup(e.target.value)}>
          <option value="">All groups</option>
          <option value={UNGROUPED}>No group</option>
          {(groups.data ?? []).map((g) => (
            <option key={g.id} value={g.id}>
              {g.name}
            </option>
          ))}
        </select>
        <label className="visually-hidden" htmlFor="pc-status">
          Status
        </label>
        <select id="pc-status" value={status} onChange={(e) => setStatus(e.target.value as ComputerStatus | '')}>
          <option value="">Any status</option>
          <option value="online">Online</option>
          <option value="offline">Offline</option>
          <option value="revoked">Revoked</option>
        </select>
        {filtered && (
          <button
            className="btn-link"
            onClick={() => {
              setQ('');
              setGroup('');
              setStatus('');
            }}
          >
            Clear filters
          </button>
        )}
      </div>

      {list.error ? (
        <div className="table-wrap">
          <ErrorState error={list.error} onRetry={list.reload} />
        </div>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th scope="col">Name</th>
                <th scope="col">ID</th>
                <th scope="col" className="hide-sm">
                  OS
                </th>
                <th scope="col" className="hide-md">
                  Version
                </th>
                <th scope="col">Group</th>
                <th scope="col" className="hide-sm">
                  Last seen
                </th>
                {canManage && (
                  <th scope="col">
                    <span className="visually-hidden">Actions</span>
                  </th>
                )}
              </tr>
            </thead>
            {list.loading && !list.data ? (
              <SkeletonRows cols={canManage ? 7 : 6} />
            ) : (
              <tbody>
                {rows.map((c) => (
                  <ComputerRow
                    key={c.id}
                    c={c}
                    groups={groups.data ?? []}
                    groupName={groupName}
                    canManage={canManage}
                    editing={editing === c.id}
                    onEdit={() => setEditing(c.id)}
                    onCancel={() => setEditing(null)}
                    onRename={(n) => rename(c, n)}
                    onMove={(g) => move(c, g)}
                    onRevoke={() => setRevoking(c)}
                  />
                ))}
              </tbody>
            )}
          </table>
          {list.data && rows.length === 0 && (
            <Empty title={filtered ? 'No computers match' : 'No computers yet'}>
              <p>
                {filtered
                  ? 'Try another search or clear the filters.'
                  : canManage
                    ? 'Create an enrollment token, then install Rconn on each computer with it.'
                    : 'An admin adds computers to this company.'}
              </p>
            </Empty>
          )}
        </div>
      )}

      <ConfirmDialog
        open={revoking !== null}
        title={`Revoke ${revoking?.name ?? ''}?`}
        confirm="Revoke"
        body={
          <p>
            It is removed from this company, live sessions to it end now, and it can’t be reached again until it is
            enrolled with a new token.
          </p>
        }
        onClose={() => setRevoking(null)}
        onConfirm={async () => {
          if (!revoking) return;
          const c = await api.revokeComputer(org.org_id, revoking.id);
          replace(c);
          toast(`${c.name} revoked`);
          groups.reload();
        }}
      />
    </>
  );
}

function ComputerRow({
  c,
  groups,
  groupName,
  canManage,
  editing,
  onEdit,
  onCancel,
  onRename,
  onMove,
  onRevoke,
}: {
  c: Computer;
  groups: Group[];
  groupName: Map<string, string>;
  canManage: boolean;
  editing: boolean;
  onEdit: () => void;
  onCancel: () => void;
  onRename: (name: string) => void;
  onMove: (groupId: string) => void;
  onRevoke: () => void;
}) {
  const revoked = c.revoked_at !== null;
  const input = useRef<HTMLInputElement>(null);
  const [draft, setDraft] = useState(c.name);
  useEffect(() => {
    if (editing) {
      setDraft(c.name);
      requestAnimationFrame(() => input.current?.select());
    }
  }, [editing, c.name]);
  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') onRename(draft);
    if (e.key === 'Escape') onCancel();
  };
  const statusText = revoked ? 'Revoked' : c.online ? 'Online' : 'Offline';
  return (
    <tr className={revoked ? 'is-revoked' : undefined} data-testid={`computer-${c.short_code.replace(/\s/g, '')}`}>
      <td>
        <div className="cell-name">
          <span
            className={`dot ${revoked ? '' : c.online ? 'on' : 'off'}`}
            title={statusText}
            aria-label={statusText}
            role="img"
          />
          {editing ? (
            <>
              <label className="visually-hidden" htmlFor={`rn-${c.id}`}>
                New name for {c.name}
              </label>
              <input
                id={`rn-${c.id}`}
                ref={input}
                value={draft}
                maxLength={64}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={onKey}
              />
              <button className="btn btn-sm" onClick={() => onRename(draft)}>
                Save
              </button>
              <button className="btn btn-quiet btn-sm" onClick={onCancel}>
                Cancel
              </button>
            </>
          ) : (
            <span>
              <span className="name">{c.name}</span>
              {revoked && <span className="badge danger" style={{ marginLeft: '0.4rem' }}>Revoked</span>}
              {!revoked && c.needs_update && (
                <span className="badge warn" style={{ marginLeft: '0.4rem' }} title={`Running ${c.version}`}>
                  Needs update
                </span>
              )}
              <span className="sub">{c.hostname}</span>
            </span>
          )}
        </div>
      </td>
      <td className="id">{c.short_code}</td>
      <td className="hide-sm">{c.os}</td>
      <td className="hide-md mono">{c.version}</td>
      <td>
        {canManage && !revoked ? (
          <>
            <label className="visually-hidden" htmlFor={`grp-${c.id}`}>
              Group for {c.name}
            </label>
            <select id={`grp-${c.id}`} value={c.group_id ?? ''} onChange={(e) => onMove(e.target.value)}>
              <option value="">No group</option>
              {groups.map((g) => (
                <option key={g.id} value={g.id}>
                  {g.name}
                </option>
              ))}
            </select>
          </>
        ) : c.group_id ? (
          groupName.get(c.group_id) ?? '—'
        ) : (
          <span className="muted">No group</span>
        )}
      </td>
      <td className="hide-sm nowrap" title={dateTime(c.last_seen)}>
        {revoked ? `Revoked ${relTime(c.revoked_at)}` : c.online ? 'Now' : relTime(c.last_seen)}
      </td>
      {canManage && (
        <td className="actions">
          {!revoked && !editing && (
            <>
              <button className="btn-link" onClick={onEdit} aria-label={`Rename ${c.name}`}>
                Rename
              </button>
              <button className="btn-link danger" onClick={onRevoke} aria-label={`Revoke ${c.name}`}>
                Revoke
              </button>
            </>
          )}
        </td>
      )}
    </tr>
  );
}
