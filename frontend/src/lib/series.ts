// Client-side data layer for time series. The backend handles bucket
// aggregation; the frontend just asks for "fit this window into roughly
// N samples" and caches the response keyed on (host, bucket-aligned range,
// budget) so adjacent pans hit the cache.

import { api, type SeriesResp } from './api';

const DEFAULT_TARGET = 600;

/**
 * Approximate pixel width per bucket the chart aims for. The median line is
 * rendered at lineWidth=4 px, so a bucket roughly this wide produces visually
 * square cells (bucket pixel width ~= median segment height) - which reads
 * cleanly and matches Smokeping's classic look. Callers compute their sample
 * budget as `floor(chartWidth / BUCKET_PX)`.
 */
export const BUCKET_PX = 5;

/**
 * Translate a chart's pixel width into the sample count to ask for, applying
 * sane caps so a 200 px panel doesn't ship 33 samples and a 4 k display
 * doesn't pull 700 buckets per panel.
 */
export function samplesForWidth(chartPx: number): number {
  return Math.max(50, Math.min(2000, Math.round(chartPx / BUCKET_PX)));
}

const cache = new Map<string, SeriesResp>();
/** Dedup keyed by full cache key, not by host. Two callers asking for the
 *  same (host, bucket-aligned range, budget) at the same time share one
 *  network request. Two callers asking for DIFFERENT ranges of the same
 *  host get DIFFERENT entries here, so neither cancels the other - the
 *  previous design (one controller per host) used to abort whichever
 *  request arrived first when SmallMultiples fired its four panels in
 *  parallel, leaving 3 panels stuck on "Loading…". */
const pending = new Map<string, Promise<SeriesResp>>();

function alignDown(value: number, step: number): number {
  return Math.floor(value / step) * step;
}
function alignUp(value: number, step: number): number {
  return Math.ceil(value / step) * step;
}

export interface SeriesRequest {
  hostUuid: string;
  fromSecs: number;
  toSecs: number;
  /** Roughly how many samples we want on screen. Defaults to 600. The server
   *  buckets raw chunk data down to this count using NaN-aware per-percentile
   *  consolidation, so a 90-day window with a 5-second probe interval doesn't
   *  ship 1.5 M samples to the browser. */
  targetSamples?: number;
}

export async function loadSeries(req: SeriesRequest): Promise<SeriesResp> {
  const span = Math.max(1, req.toSecs - req.fromSecs);
  const target = Math.max(50, req.targetSamples ?? DEFAULT_TARGET);
  // The server will produce buckets of `ceil(span / target)` seconds. Align
  // the request to the same width so adjacent pans land on identical bucket
  // boundaries and the response is byte-for-byte cacheable.
  const bucketSecs = Math.max(1, Math.ceil(span / target));
  const from = alignDown(req.fromSecs, bucketSecs);
  const to = alignUp(req.toSecs, bucketSecs);
  const key = `${req.hostUuid}:${target}:${from}:${to}`;

  const hit = cache.get(key);
  if (hit) return hit;

  const existing = pending.get(key);
  if (existing) return existing;

  // Eagerly stash the promise so two synchronous callers race to the
  // same Map entry and end up sharing one fetch. The IIFE captures the
  // closure over its own placeholder so the finally clause can identify
  // its own entry vs a later overwrite.
  let self!: Promise<SeriesResp>;
  self = (async () => {
    try {
      const resp = await api.getSeries(req.hostUuid, from, to, target);
      cache.set(key, resp);
      return resp;
    } finally {
      if (pending.get(key) === self) pending.delete(key);
    }
  })();
  pending.set(key, self);
  return self;
}

/** Kept for callers that still import it; now a no-op because in-flight
 *  requests are never cancelled. Letting them finish and populate the
 *  cache is cheaper than aborting and re-firing on the next scroll-back. */
export function cancelInflight(_hostUuid: string) {
  // intentionally empty
}

/** Kept for compatibility; with no aborts, AbortError should never come
 *  out of loadSeries anymore, but callers still pattern-match this so
 *  their catch arms stay simple. */
export function isAbortError(e: unknown): boolean {
  if (e instanceof DOMException && e.name === 'AbortError') return true;
  if (e instanceof Error && e.name === 'AbortError') return true;
  return false;
}

export function clearCache() {
  cache.clear();
}
