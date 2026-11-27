import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { poll, type PollEnv } from '../src/lib/poll';

function fakeDoc(state: 'visible' | 'hidden') {
  const target = new EventTarget();
  const doc = {
    visibilityState: state as DocumentVisibilityState,
    addEventListener: target.addEventListener.bind(target),
    removeEventListener: target.removeEventListener.bind(target),
  };
  const set = (s: 'visible' | 'hidden') => {
    doc.visibilityState = s;
    target.dispatchEvent(new Event('visibilitychange'));
  };
  return { doc: doc as unknown as PollEnv['doc'], set };
}

const env = (doc: PollEnv['doc']): PollEnv => ({
  doc,
  setInterval: (fn, ms) => setInterval(fn, ms),
  clearInterval: (id) => clearInterval(id as ReturnType<typeof setInterval>),
});

describe('poll', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('ticks every interval while visible', () => {
    const tick = vi.fn();
    const { doc } = fakeDoc('visible');
    const stop = poll(tick, 10_000, env(doc));
    expect(tick).not.toHaveBeenCalled();
    vi.advanceTimersByTime(9_999);
    expect(tick).toHaveBeenCalledTimes(0);
    vi.advanceTimersByTime(1);
    expect(tick).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(20_000);
    expect(tick).toHaveBeenCalledTimes(3);
    stop();
    vi.advanceTimersByTime(60_000);
    expect(tick).toHaveBeenCalledTimes(3);
  });

  it('pauses while hidden and catches up when shown again', () => {
    const tick = vi.fn();
    const { doc, set } = fakeDoc('visible');
    const stop = poll(tick, 10_000, env(doc));
    set('hidden');
    vi.advanceTimersByTime(60_000);
    expect(tick).not.toHaveBeenCalled();
    set('visible');
    expect(tick).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(10_000);
    expect(tick).toHaveBeenCalledTimes(2);
    // A second "visible" event doesn't start a second timer.
    set('visible');
    vi.advanceTimersByTime(10_000);
    expect(tick).toHaveBeenCalledTimes(3);
    stop();
    set('visible');
    expect(tick).toHaveBeenCalledTimes(3);
  });

  it('starts hidden without ticking', () => {
    const tick = vi.fn();
    const { doc, set } = fakeDoc('hidden');
    poll(tick, 10_000, env(doc));
    vi.advanceTimersByTime(30_000);
    expect(tick).not.toHaveBeenCalled();
    set('visible');
    expect(tick).toHaveBeenCalledTimes(1);
  });
});
