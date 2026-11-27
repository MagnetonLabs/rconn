import { useCallback, useEffect, useMemo, useRef, useState, type MouseEvent } from 'react';
import * as api from '../api/audit';
import type { AuditEvent, AuditFilter } from '../api/audit';
import { AUDIT_KINDS } from '../api/audit';
import { ApiError } from '../api/http';
import { useOrg, useCan } from '../lib/org';
import { dateTime, fromLocalInput, relTime } from '../lib/format';
import { Empty, ErrorState, SkeletonRows, errorMessage, useToast } from '../components/ui';

const RANGES: [string, string, number | null][] = [
  ['24h', 'Last 24 hours', 1],
  ['7d', 'Last 7 days', 7],
  ['30d', 'Last 30 days', 30],
  ['90d', 'Last 90 days', 90],
  ['all', 'All time', null],
  ['custom', 'Custom range', null],
];

const KIND_LABEL: Record<string, string> = {
  'session.start': 'Session started',
  'session.end': 'Session ended',
  'file.upload': 'File sent',
  'file.download': 'File received',
  'computer.enrolled': 'Computer enrolled',
  'computer.renamed': 'Computer renamed',
  'computer.revoked': 'Computer revoked',
  'person.invited': 'Person invited',
  'person.role_changed': 'Role changed',
  'person.removed': 'Person removed',
  'policy.changed': 'Policy changed',
  sign_in: 'Signed in',
  'sign_in.failed': 'Sign-in failed',
  'sso.changed': 'SSO changed',
  'token.created': 'Token created',
  'token.deleted': 'Token deleted',
};

function kindClass(k: string) {
  if (k === 'sign_in.failed' || k === 'computer.revoked' || k === 'person.removed') return 'danger';
  if (k.startsWith('session.')) return 'ok';
  if (k.startsWith('file.')) return '';
  return 'plain';
}

function detailText(e: AuditEvent): string {
  const d = e.detail ?? {};
  if (typeof d.duration_s === 'number') {
    const m = Math.round(d.duration_s / 60);
    return m >= 1 ? `${m} min` : `${d.duration_s} s`;
  }
  if (typeof d.file === 'string') return `${d.file}${typeof d.bytes === 'number' ? ` (${(d.bytes / 1e6).toFixed(1)} MB)` : ''}`;
  if (typeof d.from === 'string' && typeof d.to === 'string') return `${d.from} → ${d.to}`;
  if (typeof d.method === 'string') return d.method.replace('_', ' ');
  if (typeof d.role === 'string') return d.role;
  if (typeof d.reason === 'string') return d.reason.replace('_', ' ');
  if (typeof d.field === 'string') return d.field.replace('_', ' ');
  const keys = Object.keys(d);
  return keys.length ? keys.map((k) => `${k}: ${String(d[k])}`).join(', ') : '';
}

function useDebounced<T>(v: T, ms: number): T {
  const [d, setD] = useState(v);
  useEffect(() => {
    const t = setTimeout(() => setD(v), ms);
    return () => clearTimeout(t);
  }, [v, ms]);
  return d;
}

export default function Audit() {
  const org = useOrg();
  const allowed = useCan('audit.view');
  const toast = useToast();
  const [range, setRange] = useState('7d');
  const [customFrom, setCustomFrom] = useState('');
  const [customTo, setCustomTo] = useState('');
  const [kind, setKind] = useState('');
  const [q, setQ] = useState('');
  const dq = useDebounced(q.trim(), 250);

  const filter = useMemo<AuditFilter>(() => {
    const f: AuditFilter = { kind: kind || undefined, q: dq || undefined };
    const r = RANGES.find((x) => x[0] === range);
    if (range === 'custom') {
      f.from = fromLocalInput(customFrom);
      f.to = fromLocalInput(customTo);
    } else if (r && r[2] !== null) {
      f.from = new Date(Date.now() - r[2] * 86_400_000).toISOString();
    }
    return f;
  }, [range, customFrom, customTo, kind, dq]);

  const [items, setItems] = useState<AuditEvent[] | null>(null);
  const [next, setNext] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const gen = useRef(0);

  const loadFirst = useCallback(() => {
    const my = ++gen.current;
    setItems(null);
    setError(null);
    api.listAudit(org.org_id, filter).then(
      (p) => {
        if (my !== gen.current) return;
        setItems(p.items);
        setNext(p.next);
      },
      (e) => my === gen.current && setError(e),
    );
  }, [org.org_id, filter]);

  useEffect(() => {
    if (allowed) loadFirst();
  }, [allowed, loadFirst]);

  const loadMore = useCallback(async () => {
    if (!next || loadingMore) return;
    const my = gen.current;
    setLoadingMore(true);
    try {
      const p = await api.listAudit(org.org_id, filter, next);
      if (my !== gen.current) return;
      setItems((xs) => [...(xs ?? []), ...p.items]);
      setNext(p.next);
    } catch (e) {
      toast(errorMessage(e), 'error');
    } finally {
      setLoadingMore(false);
    }
  }, [next, loadingMore, org.org_id, filter, toast]);

  const sentinel = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = sentinel.current;
    if (!el || !next || typeof IntersectionObserver === 'undefined') return;
    const io = new IntersectionObserver((es) => es.some((e) => e.isIntersecting) && void loadMore(), { rootMargin: '200px' });
    io.observe(el);
    return () => io.disconnect();
  }, [next, loadMore]);

  const csvHref = api.auditCsvUrl(org.org_id, filter);
  const onExport = async (e: MouseEvent<HTMLAnchorElement>) => {
    // The mock backend lives inside fetch, so a plain link can't reach it: download through fetch.
    if (import.meta.env.MODE !== 'mock') return;
    e.preventDefault();
    const res = await fetch(csvHref);
    const blob = await res.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = 'rconn-audit.csv';
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  };

  if (!allowed) {
    return (
      <>
        <div className="page-head">
          <h1>Audit</h1>
        </div>
        <ErrorState error={new ApiError(403, 'forbidden', '')} />
      </>
    );
  }

  return (
    <>
      <div className="page-head">
        <h1>
          Audit{items && <span className="count">{items.length}{next ? '+' : ''}</span>}
        </h1>
        <a className="btn btn-quiet" href={csvHref} download="rconn-audit.csv" onClick={onExport}>
          Export CSV
        </a>
      </div>
      <div className="toolbar" role="search">
        <label className="visually-hidden" htmlFor="au-q">
          Search events
        </label>
        <input id="au-q" type="search" placeholder="Search person, computer, IP" value={q} onChange={(e) => setQ(e.target.value)} />
        <label className="visually-hidden" htmlFor="au-range">
          Time range
        </label>
        <select id="au-range" value={range} onChange={(e) => setRange(e.target.value)}>
          {RANGES.map(([v, l]) => (
            <option key={v} value={v}>
              {l}
            </option>
          ))}
        </select>
        {range === 'custom' && (
          <>
            <label htmlFor="au-from" className="muted">
              From
            </label>
            <input id="au-from" type="datetime-local" value={customFrom} onChange={(e) => setCustomFrom(e.target.value)} />
            <label htmlFor="au-to" className="muted">
              to
            </label>
            <input id="au-to" type="datetime-local" value={customTo} onChange={(e) => setCustomTo(e.target.value)} />
          </>
        )}
        <label className="visually-hidden" htmlFor="au-kind">
          Event type
        </label>
        <select id="au-kind" value={kind} onChange={(e) => setKind(e.target.value)}>
          <option value="">All events</option>
          {AUDIT_KINDS.map((k) => (
            <option key={k} value={k}>
              {KIND_LABEL[k] ?? k}
            </option>
          ))}
        </select>
      </div>

      <div className="table-wrap">
        {error ? (
          <ErrorState error={error} onRetry={loadFirst} />
        ) : (
          <table>
            <thead>
              <tr>
                <th scope="col">Time</th>
                <th scope="col">Event</th>
                <th scope="col">Who</th>
                <th scope="col">Target</th>
                <th scope="col" className="hide-md">
                  Details
                </th>
                <th scope="col" className="hide-sm">
                  IP
                </th>
              </tr>
            </thead>
            {items === null ? (
              <SkeletonRows cols={6} rows={10} />
            ) : (
              <tbody>
                {items.map((e) => (
                  <tr key={e.id}>
                    <td className="nowrap" title={relTime(e.at)}>
                      {dateTime(e.at)}
                    </td>
                    <td>
                      <span className={`badge ${kindClass(e.kind)}`}>{KIND_LABEL[e.kind] ?? e.kind}</span>
                    </td>
                    <td>
                      {e.actor.label}
                      {e.actor.type !== 'user' && <span className="sub">{e.actor.type}</span>}
                    </td>
                    <td>{e.target?.label ?? <span className="muted">—</span>}</td>
                    <td className="hide-md muted">{detailText(e)}</td>
                    <td className="hide-sm mono">{e.ip ?? '—'}</td>
                  </tr>
                ))}
              </tbody>
            )}
          </table>
        )}
        {items?.length === 0 && (
          <Empty title="No events match">
            <p>Try a longer time range or clear the search.</p>
          </Empty>
        )}
      </div>
      {next && (
        <div ref={sentinel} style={{ textAlign: 'center', marginTop: '0.75rem' }}>
          <button className="btn btn-quiet" onClick={() => void loadMore()} disabled={loadingMore}>
            {loadingMore ? 'Loading…' : 'Load more'}
          </button>
        </div>
      )}
    </>
  );
}
