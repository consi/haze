<script lang="ts">
  import uPlot from 'uplot';
  import 'uplot/dist/uPlot.min.css';
  import { onDestroy, onMount } from 'svelte';
  import type { SeriesResp } from '$lib/api';
  import { startViewportTracking, viewport } from '$lib/viewport.svelte';

  let {
    series,
    height = 220,
    onZoom,
    xMin,
    xMax
  }: {
    series: SeriesResp;
    height?: number;
    onZoom?: (fromSecs: number, toSecs: number) => void;
    /**
     * Force the x-axis to span exactly `[xMin, xMax]` regardless of how much
     * data actually exists. Used by the multi-period view so a panel that
     * asks for "last 90 days" still draws a 90-day-wide chart even when the
     * host only has a few minutes of data so far - empty area on the left,
     * data flush against the right edge.
     */
    xMin?: number;
    xMax?: number;
  } = $props();

  let container: HTMLDivElement | undefined = $state();
  let wrapper: HTMLDivElement | undefined = $state();
  let plot: uPlot | null = null;
  let lastXMin: number | undefined;
  let lastXMax: number | undefined;

  type TipSample = {
    ts: number;
    median: number | null;
    p25: number | null;
    p75: number | null;
    p2_5: number | null;
    p97_5: number | null;
    loss_pct: number | null;
  };
  let tip = $state<{
    visible: boolean;
    x: number;
    y: number;
    flipRight: boolean;
    flipDown: boolean;
    sample: TipSample | null;
  }>({ visible: false, x: 0, y: 0, flipRight: false, flipDown: false, sample: null });

  // Touch interaction state machine. uPlot's built-in cursor drag is
  // mouse-only (mousedown/mousemove/mouseup) and modern browsers don't
  // synthesise mouse drag events from touch, so on mobile we drive the
  // gestures ourselves with pointer events:
  //
  // - 'pending'  — finger is down; we're waiting to see if it becomes a
  //                tap, long-press, or drag. Tooltip suppressed.
  // - 'tooltip'  — held still for ~400 ms; tooltip appears and follows
  //                the finger horizontally. Page scrolls if user slides
  //                vertically (handled by touch-action: pan-y below).
  // - 'drag'     — moved past the threshold before long-press; we draw
  //                a horizontal selection band and zoom to it on release.
  // - 'idle'     — nothing pressed. Tooltip from the previous gesture
  //                stays visible until the next pointerdown.
  //
  // A quick tap (released < long-press, < threshold movement) checks the
  // double-tap window: two taps within 300 ms / 30 px → zoom out.
  type TouchState = 'idle' | 'pending' | 'tooltip' | 'drag';
  // $state so the SmokeChart effects below can gate live-mode redraws on
  // it — without that, a 1 s live tick from the parent host page shifts
  // the chart's x-scale out from under the user's finger mid-drag and
  // the zoom-to-band on release lands on the wrong time range.
  let touchState = $state<TouchState>('idle');
  let longPressTimer: ReturnType<typeof setTimeout> | null = null;
  let touchStartPos: { x: number; y: number } | null = null;
  let touchDragRect = $state<{ startX: number; endX: number } | null>(null);
  let lastTapTime = 0;
  let lastTapPos: { x: number; y: number } | null = null;
  const LONG_PRESS_MS = 400;
  const MOVE_THRESHOLD_PX = 10;
  const DOUBLE_TAP_MS = 300;
  const DOUBLE_TAP_PX = 30;

  onMount(() => {
    const stopViewport = startViewportTracking();
    // Document-level capture-phase listeners so we update touchState
    // BEFORE uPlot's element-level listeners fire. Otherwise the
    // setCursor hook below would see touchState === 'idle' on the very
    // first cursor update of a touch session and let the tooltip flash.
    document.addEventListener('pointerdown', onPointerDownCapture, true);
    document.addEventListener('pointermove', onPointerMoveCapture, true);
    document.addEventListener('pointerup', onPointerUpCapture, true);
    document.addEventListener('pointercancel', onPointerCancelCapture, true);
    return () => {
      stopViewport();
      document.removeEventListener('pointerdown', onPointerDownCapture, true);
      document.removeEventListener('pointermove', onPointerMoveCapture, true);
      document.removeEventListener('pointerup', onPointerUpCapture, true);
      document.removeEventListener('pointercancel', onPointerCancelCapture, true);
    };
  });

  // Locale-aware date formatters.
  const fmtDayMonth = new Intl.DateTimeFormat(undefined, {
    day: 'numeric',
    month: 'short'
  });
  const fmtMonthYear = new Intl.DateTimeFormat(undefined, {
    month: 'short',
    year: 'numeric'
  });
  const fmtYear = new Intl.DateTimeFormat(undefined, { year: 'numeric' });
  const fmtTooltipDate = new Intl.DateTimeFormat(undefined, {
    day: 'numeric',
    month: 'short',
    year: 'numeric'
  });

  const LOSS_PALETTE: readonly string[] = [
    '#00cc00', '#00b1ff', '#5959ff', '#b300b3',
    '#ff5cff', '#ff950c', '#ff0000', '#6b1717'
  ];
  function lossColor(loss: number | null | undefined): string {
    const v = loss ?? 0;
    if (v <= 0) return LOSS_PALETTE[0];
    if (v <= 5) return LOSS_PALETTE[1];
    if (v <= 10) return LOSS_PALETTE[2];
    if (v <= 15) return LOSS_PALETTE[3];
    if (v <= 25) return LOSS_PALETTE[4];
    if (v <= 50) return LOSS_PALETTE[5];
    if (v <= 95) return LOSS_PALETTE[6];
    return LOSS_PALETTE[7];
  }
  const UNREACHABLE_LOSS_PCT = 99.5;
  function isUnreachable(loss: number | null | undefined): boolean {
    return loss != null && loss >= UNREACHABLE_LOSS_PCT;
  }

  function pack(s: SeriesResp): uPlot.AlignedData {
    const n = s.samples.length;
    const xs = new Array<number>(n);
    const med = new Array<number | null>(n);
    const p25 = new Array<number | null>(n);
    const p75 = new Array<number | null>(n);
    const p2_5 = new Array<number | null>(n);
    const p97_5 = new Array<number | null>(n);
    const loss = new Array<number | null>(n);
    for (let i = 0; i < n; i++) {
      const p = s.samples[i];
      xs[i] = p.ts;
      med[i] = p.median ?? null;
      p25[i] = p.p25 ?? null;
      p75[i] = p.p75 ?? null;
      p2_5[i] = p.p2_5 ?? null;
      p97_5[i] = p.p97_5 ?? null;
      loss[i] = p.loss_pct ?? null;
    }
    return [xs, med, p25, p75, p2_5, p97_5, loss] as uPlot.AlignedData;
  }

  function cssVar(name: string, fallback: string): string {
    if (typeof window === 'undefined') return fallback;
    const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return v || fallback;
  }

  function withAlpha(color: string, alpha: number): string {
    if (color.startsWith('#') && (color.length === 7 || color.length === 4)) {
      const hex =
        color.length === 4
          ? `#${color[1]}${color[1]}${color[2]}${color[2]}${color[3]}${color[3]}`
          : color;
      const r = parseInt(hex.slice(1, 3), 16);
      const g = parseInt(hex.slice(3, 5), 16);
      const b = parseInt(hex.slice(5, 7), 16);
      return `rgba(${r},${g},${b},${alpha.toFixed(3)})`;
    }
    return color;
  }

  function clip(u: uPlot) {
    u.ctx.beginPath();
    u.ctx.rect(u.bbox.left, u.bbox.top, u.bbox.width, u.bbox.height);
    u.ctx.clip();
  }

  function bandDrawer(
    lowIdx: number,
    highIdx: number,
    fill: string,
    alpha: number,
    getResolutionSecs: () => number
  ) {
    return (u: uPlot, _seriesIdx: number, idx0: number, idx1: number) => {
      const xs = u.data[0] as number[];
      const lows = u.data[lowIdx] as (number | null)[];
      const highs = u.data[highIdx] as (number | null)[];
      const losses = u.data[6] as (number | null)[];
      const ctx = u.ctx;
      const halfBucket = getResolutionSecs() / 2;
      ctx.save();
      clip(u);
      ctx.fillStyle = withAlpha(fill, alpha);
      type Pt = { idx: number; x: number; low: number; high: number };
      let chain: Pt[] = [];
      // At a chain endpoint (last reachable before an unreachable run, or
      // first after) we extend to the midpoint with the actual neighbour
      // data point rather than the conservative ±halfBucket. That makes
      // the band meet the unreachable rect exactly even when the data is
      // non-uniformly spaced (e.g. probe slipped a beat around the
      // outage). Only at the absolute start/end of the data array does
      // the halfBucket fallback apply.
      const leftEdgeX = (k: number, arr: Pt[]) => {
        const p = arr[k];
        if (k > 0) return (arr[k - 1].x + p.x) / 2;
        if (p.idx > 0) return (xs[p.idx - 1] + p.x) / 2;
        return p.x - halfBucket;
      };
      const rightEdgeX = (k: number, arr: Pt[]) => {
        const p = arr[k];
        if (k < arr.length - 1) return (p.x + arr[k + 1].x) / 2;
        if (p.idx < xs.length - 1) return (p.x + xs[p.idx + 1]) / 2;
        return p.x + halfBucket;
      };
      const drawPolygon = (arr: Pt[]) => {
        const n = arr.length;
        if (n === 0) return;
        const path = new Path2D();
        for (let k = 0; k < n; k++) {
          const p = arr[k];
          const lx = u.valToPos(leftEdgeX(k, arr), 'x', true);
          const rx = u.valToPos(rightEdgeX(k, arr), 'x', true);
          const yTop = u.valToPos(p.high, 'y', true);
          if (k === 0) path.moveTo(lx, yTop);
          else path.lineTo(lx, yTop);
          path.lineTo(rx, yTop);
        }
        for (let k = n - 1; k >= 0; k--) {
          const p = arr[k];
          const lx = u.valToPos(leftEdgeX(k, arr), 'x', true);
          const rx = u.valToPos(rightEdgeX(k, arr), 'x', true);
          const yBot = u.valToPos(p.low, 'y', true);
          path.lineTo(rx, yBot);
          path.lineTo(lx, yBot);
        }
        path.closePath();
        ctx.fill(path);
      };
      const flush = () => {
        if (chain.length) {
          drawPolygon(chain);
          chain = [];
        }
      };
      for (let i = idx0; i <= idx1; i++) {
        const lo = lows[i];
        const hi = highs[i];
        if (lo != null && hi != null) {
          chain.push({ idx: i, x: xs[i], low: lo, high: hi });
        } else if (isUnreachable(losses[i])) {
          flush();
        }
      }
      flush();
      ctx.restore();
      return null;
    };
  }

  function medianDrawer(getResolutionSecs: () => number) {
    return (u: uPlot, _seriesIdx: number, idx0: number, idx1: number) => {
      const xs = u.data[0] as number[];
      const med = u.data[1] as (number | null)[];
      const losses = u.data[6] as (number | null)[];
      const ctx = u.ctx;
      const halfBucket = getResolutionSecs() / 2;
      ctx.save();
      clip(u);
      ctx.lineCap = 'butt';
      ctx.lineJoin = 'miter';
      type Pt = { idx: number; x: number; y: number };
      let chain: Pt[] = [];
      // Same logic as bandDrawer: chain endpoints meet the unreachable
      // rect's edge (midpoint to the neighbouring data point in xs)
      // rather than falling back to ±halfBucket, which left a visible
      // gap when probe spacing was non-uniform around an outage.
      const leftEdgeX = (k: number, arr: Pt[]) => {
        const p = arr[k];
        if (k > 0) return (arr[k - 1].x + p.x) / 2;
        if (p.idx > 0) return (xs[p.idx - 1] + p.x) / 2;
        return p.x - halfBucket;
      };
      const rightEdgeX = (k: number, arr: Pt[]) => {
        const p = arr[k];
        if (k < arr.length - 1) return (p.x + arr[k + 1].x) / 2;
        if (p.idx < xs.length - 1) return (p.x + xs[p.idx + 1]) / 2;
        return p.x + halfBucket;
      };
      const drawChain = (arr: Pt[]) => {
        ctx.lineWidth = 2;
        ctx.strokeStyle = '#000';
        for (let k = 0; k < arr.length - 1; k++) {
          const p = arr[k];
          const next = arr[k + 1];
          const rightX = (p.x + next.x) / 2;
          const rx = u.valToPos(rightX, 'x', true);
          const yPx = u.valToPos(p.y, 'y', true);
          const nextY = u.valToPos(next.y, 'y', true);
          ctx.beginPath();
          ctx.moveTo(rx, yPx);
          ctx.lineTo(rx, nextY);
          ctx.stroke();
        }
        ctx.lineWidth = 4;
        for (let k = 0; k < arr.length; k++) {
          const p = arr[k];
          const yPx = u.valToPos(p.y, 'y', true);
          const lx = u.valToPos(leftEdgeX(k, arr), 'x', true);
          const rx = u.valToPos(rightEdgeX(k, arr), 'x', true);
          ctx.strokeStyle = lossColor(losses[p.idx]);
          ctx.beginPath();
          ctx.moveTo(lx, yPx);
          ctx.lineTo(rx, yPx);
          ctx.stroke();
        }
      };
      const flush = () => {
        if (chain.length) {
          drawChain(chain);
          chain = [];
        }
      };
      for (let i = idx0; i <= idx1; i++) {
        const v = med[i];
        if (v == null) {
          if (isUnreachable(losses[i])) flush();
          continue;
        }
        chain.push({ idx: i, x: xs[i], y: v });
      }
      flush();
      ctx.restore();
      return null;
    };
  }

  function unreachableDrawer(getResolutionSecs: () => number) {
    return (u: uPlot, _seriesIdx: number, idx0: number, idx1: number) => {
      const xs = u.data[0] as number[];
      const losses = u.data[6] as (number | null)[];
      const ctx = u.ctx;
      ctx.save();
      clip(u);
      ctx.fillStyle = withAlpha(cssVar('--unreachable', '#5d2e2e'), 0.4);
      // Edges of the wash align with bucket boundaries instead of the
      // chart's bbox. Otherwise a host that's been unreachable since
      // boot would paint the empty area to the left of its first sample
      // (which is BEFORE any data existed) the same red as the actual
      // outage. Same for the right edge when the pinned window extends
      // past the latest sample.
      const halfBucketSecs = getResolutionSecs() / 2;
      let runStart = -1;
      const flush = (start: number, end: number) => {
        const leftX =
          start > 0 ? (xs[start - 1] + xs[start]) / 2 : xs[start] - halfBucketSecs;
        const rightX =
          end < xs.length - 1 ? (xs[end] + xs[end + 1]) / 2 : xs[end] + halfBucketSecs;
        const lx = u.valToPos(leftX, 'x', true);
        const rx = u.valToPos(rightX, 'x', true);
        ctx.fillRect(lx, u.bbox.top, rx - lx, u.bbox.height);
      };
      for (let i = idx0; i <= idx1 + 1; i++) {
        let inRun: boolean;
        if (i > idx1) {
          inRun = false;
        } else {
          const v = losses[i];
          if (isUnreachable(v)) inRun = true;
          else if (v == null) inRun = runStart !== -1;
          else inRun = false;
        }
        if (inRun && runStart === -1) runStart = i;
        else if (!inRun && runStart !== -1) {
          flush(runStart, i - 1);
          runStart = -1;
        }
      }
      ctx.restore();
      return null;
    };
  }

  function formatMs(v: number | null): string {
    if (v == null || Number.isNaN(v)) return '-';
    if (v < 1) return `${v.toFixed(2)} ms`;
    if (v < 10) return `${v.toFixed(2)} ms`;
    if (v < 100) return `${v.toFixed(1)} ms`;
    return `${Math.round(v)} ms`;
  }
  function formatLoss(v: number | null): string {
    if (v == null || Number.isNaN(v)) return '-';
    if (v === 0) return '0%';
    if (v < 1) return `${v.toFixed(1)}%`;
    return `${Math.round(v)}%`;
  }
  function zoomOutOneWindow() {
    if (!onZoom) return;
    const span = series.to - series.from;
    if (span <= 0) return;
    // Zoom out: keep the right edge ("now"-side) anchored and extend the
    // range backwards by another full span. That doubles the visible
    // window each click, naturally showing more history.
    onZoom(Math.round(series.from - span), Math.round(series.to));
  }

  function onContextMenu(e: MouseEvent) {
    e.preventDefault();
    zoomOutOneWindow();
  }

  function touchInsideWrapper(e: PointerEvent): boolean {
    if (e.pointerType !== 'touch') return false;
    if (!wrapper) return false;
    if (!(e.target instanceof Node)) return false;
    return wrapper.contains(e.target);
  }

  function onPointerDownCapture(e: PointerEvent) {
    if (!touchInsideWrapper(e)) return;
    touchState = 'pending';
    touchStartPos = { x: e.clientX, y: e.clientY };
    touchDragRect = null;
    // Hide any tooltip left over from the previous gesture immediately
    // so a fresh tap doesn't flash old data while we wait for long-press.
    tip.visible = false;
    if (longPressTimer) clearTimeout(longPressTimer);
    longPressTimer = setTimeout(() => {
      if (touchState !== 'pending') return;
      touchState = 'tooltip';
      longPressTimer = null;
      // Trigger uPlot's setCursor so the existing hook computes the
      // tooltip sample at the press location. With touchState ===
      // 'tooltip', the hook's suppression gate lets the tooltip render.
      if (plot && touchStartPos && wrapper) {
        const rect = wrapper.getBoundingClientRect();
        plot.setCursor(
          { left: touchStartPos.x - rect.left, top: touchStartPos.y - rect.top },
          false
        );
      }
    }, LONG_PRESS_MS);
  }

  function onPointerMoveCapture(e: PointerEvent) {
    if (e.pointerType !== 'touch') return;
    if (touchState === 'idle' || !touchStartPos || !wrapper) return;

    if (touchState === 'tooltip') {
      // Slide the tooltip with the finger.
      if (plot) {
        const rect = wrapper.getBoundingClientRect();
        plot.setCursor({ left: e.clientX - rect.left, top: e.clientY - rect.top }, false);
      }
      return;
    }

    const dx = e.clientX - touchStartPos.x;
    if (touchState === 'pending' && Math.abs(dx) > MOVE_THRESHOLD_PX) {
      touchState = 'drag';
      if (longPressTimer) {
        clearTimeout(longPressTimer);
        longPressTimer = null;
      }
    }
    if (touchState === 'drag') {
      const rect = wrapper.getBoundingClientRect();
      touchDragRect = {
        startX: touchStartPos.x - rect.left,
        endX: e.clientX - rect.left
      };
    }
  }

  function onPointerUpCapture(e: PointerEvent) {
    if (e.pointerType !== 'touch') return;
    if (touchState === 'idle') return;
    if (longPressTimer) {
      clearTimeout(longPressTimer);
      longPressTimer = null;
    }
    const prev = touchState;

    if (prev === 'drag' && touchDragRect && plot && onZoom) {
      const lo = Math.min(touchDragRect.startX, touchDragRect.endX);
      const hi = Math.max(touchDragRect.startX, touchDragRect.endX);
      if (hi - lo > MOVE_THRESHOLD_PX) {
        const startVal = plot.posToVal(lo, 'x');
        const endVal = plot.posToVal(hi, 'x');
        onZoom(Math.round(startVal), Math.round(endVal));
      }
    }

    if (prev === 'pending' && touchStartPos) {
      // Quick tap — possibly the second tap of a double-tap.
      const now = performance.now();
      if (lastTapPos && now - lastTapTime < DOUBLE_TAP_MS) {
        const ddx = touchStartPos.x - lastTapPos.x;
        const ddy = touchStartPos.y - lastTapPos.y;
        if (Math.hypot(ddx, ddy) < DOUBLE_TAP_PX) {
          zoomOutOneWindow();
          lastTapTime = 0;
          lastTapPos = null;
          touchState = 'idle';
          touchStartPos = null;
          touchDragRect = null;
          return;
        }
      }
      lastTapTime = now;
      lastTapPos = { x: touchStartPos.x, y: touchStartPos.y };
    }

    // Tooltip stays visible after a long-press release; next pointerdown
    // wipes it. Drag rect is purely transient.
    touchDragRect = null;
    touchStartPos = null;
    touchState = 'idle';
  }

  function onPointerCancelCapture(e: PointerEvent) {
    if (e.pointerType !== 'touch') return;
    if (longPressTimer) {
      clearTimeout(longPressTimer);
      longPressTimer = null;
    }
    touchState = 'idle';
    touchStartPos = null;
    touchDragRect = null;
  }

  function formatTipTimestamp(secs: number): string {
    const d = new Date(secs * 1000);
    const hh = d.getHours().toString().padStart(2, '0');
    const mm = d.getMinutes().toString().padStart(2, '0');
    return `${hh}:${mm} · ${fmtTooltipDate.format(d)}`;
  }

  type StatRow = { avg: number; max: number; min: number; now: number; sd: number };
  function summarise(values: (number | null | undefined)[]): StatRow | null {
    const vs: number[] = [];
    for (const v of values) {
      if (v != null && !Number.isNaN(v)) vs.push(v);
    }
    if (vs.length === 0) return null;
    let sum = 0;
    let mn = Infinity;
    let mx = -Infinity;
    for (const v of vs) {
      sum += v;
      if (v < mn) mn = v;
      if (v > mx) mx = v;
    }
    const avg = sum / vs.length;
    let variance = 0;
    for (const v of vs) variance += (v - avg) ** 2;
    const sd = Math.sqrt(variance / vs.length);
    // "now" = the most recent non-null reading (the last one in chronological order).
    let now = vs[vs.length - 1];
    for (let i = values.length - 1; i >= 0; i--) {
      const v = values[i];
      if (v != null && !Number.isNaN(v)) {
        now = v;
        break;
      }
    }
    return { avg, max: mx, min: mn, now, sd };
  }

  const medianStats = $derived(summarise(series.samples.map((s) => s.median)));
  const lossStats = $derived(summarise(series.samples.map((s) => s.loss_pct)));

  function makeOpts(width: number): uPlot.Options {
    const muted = cssVar('--muted', '#6b7480');
    const smokeOuter = cssVar('--smoke-outer', '#e2e5e7');
    const smokeInner = cssVar('--smoke-inner', '#a0a5ab');
    const noPoints = { show: false };
    const nullPaths = () => null;

    // Drawers read the current resolution lazily on each frame, so a refetch
    // that lands a different bucket size doesn't require destroying the
    // plot - we just call setData and the drawers pick up the new value.
    const getResolution = () => series.resolution_secs;
    const drawUnreachable = unreachableDrawer(getResolution);
    const drawOuter = bandDrawer(4, 5, smokeOuter, 1.0, getResolution);
    const drawInner = bandDrawer(2, 3, smokeInner, 1.0, getResolution);
    const drawMedian = medianDrawer(getResolution);

    const transparentSeries = (label: string): uPlot.Series => ({
      show: true,
      label,
      stroke: 'transparent',
      points: noPoints,
      paths: nullPaths
    });

    return {
      width,
      height,
      legend: { show: false },
      scales: {
        // x range is pinned imperatively via `setScale` after data updates,
        // not via a `range` callback - that way the live clock can advance
        // the right edge each second without forcing a destroy + recreate.
        x: { time: true },
        y: { auto: true }
      },
      series: [
        {},
        transparentSeries('median'),
        transparentSeries('p25'),
        transparentSeries('p75'),
        transparentSeries('p2.5'),
        transparentSeries('p97.5'),
        transparentSeries('loss_pct')
      ],
      axes: [
        {
          stroke: muted,
          space: 35,
          rotate: 90,
          // Labels are rotated 90 deg, so `size` is how tall the axis band
          // is along the chart edge - i.e. how many pixels of label fit
          // before the leading character gets clipped. The longest label we
          // print is HH:MM:SS (8 chars) at sub-minute zoom, which needs ~70.
          size: 72,
          values: (_u, splits, _axisIdx, _foundSpace, foundIncr) =>
            splits.map((secs) => {
              const d = new Date(secs * 1000);
              if (foundIncr >= 365 * 86400) return fmtYear.format(d);
              if (foundIncr >= 28 * 86400) return fmtMonthYear.format(d);
              if (foundIncr >= 86400) return fmtDayMonth.format(d);
              const isMidnight =
                d.getHours() === 0 && d.getMinutes() === 0 && d.getSeconds() === 0;
              if (isMidnight) return fmtDayMonth.format(d);
              const hh = d.getHours().toString().padStart(2, '0');
              const mm = d.getMinutes().toString().padStart(2, '0');
              // Sub-minute zoom: tick spacing finer than 1 min means the
              // seconds digits are what's actually changing between ticks.
              // Switch to HH:MM:SS so adjacent labels aren't identical.
              if (foundIncr < 60) {
                const ss = d.getSeconds().toString().padStart(2, '0');
                return `${hh}:${mm}:${ss}`;
              }
              return `${hh}:${mm}`;
            }),
          grid: { stroke: 'rgba(0,0,0,0.035)', width: 1 }
        },
        {
          stroke: muted,
          label: 'ms',
          space: 22,
          grid: { stroke: 'rgba(0,0,0,0.035)', width: 1 }
        }
      ],
      cursor: {
        x: true,
        y: false,
        points: { show: false },
        drag: { x: true, y: false, dist: 5 }
      },
      hooks: {
        draw: [
          (u) => {
            const xs = u.data[0] as number[];
            if (xs.length === 0) return;
            const idx0 = 0;
            const idx1 = xs.length - 1;
            drawUnreachable(u, 0, idx0, idx1);
            drawOuter(u, 0, idx0, idx1);
            drawInner(u, 0, idx0, idx1);
            drawMedian(u, 0, idx0, idx1);
          }
        ],
        setCursor: [
          (u) => {
            // Suppress the tooltip while a touch is pending or actively
            // dragging — quick taps and pinch-zoom hand-offs should NOT
            // flash the tooltip just because uPlot moved the cursor.
            if (touchState === 'pending' || touchState === 'drag') {
              tip.visible = false;
              return;
            }
            const idx = u.cursor.idx;
            const left = u.cursor.left ?? -1;
            const top = u.cursor.top ?? -1;
            const xs = u.data[0] as number[];
            if (idx == null || idx < 0 || idx >= xs.length || left < 0 || top < 0) {
              tip.visible = false;
              return;
            }
            // uPlot snaps `idx` to the nearest data point even when the
            // cursor is in dead space (e.g. an empty 90-day window with
            // only the last 5 min of data). Reject the hit if the cursor's
            // x value is more than one bucket away from the nearest
            // sample - we're hovering over empty area, not data.
            const cursorXVal = u.posToVal(left, 'x');
            const bucketSecs = Math.max(1, getResolution());
            if (Math.abs(cursorXVal - xs[idx]) > bucketSecs) {
              tip.visible = false;
              return;
            }
            // The cursor's slot may still not have a sample (sparse data
            // within the visible range). Walk backwards to the most recent
            // slot with a reading; the bands/median already extend visually
            // through these gaps, so the tooltip matches that semantic
            // instead of showing a stripe of "-".
            const med = u.data[1] as (number | null)[];
            const loss = u.data[6] as (number | null)[];
            let effIdx = idx;
            while (effIdx >= 0 && med[effIdx] == null && loss[effIdx] == null) {
              effIdx--;
            }
            if (effIdx < 0) {
              tip.visible = false;
              return;
            }
            tip.sample = {
              ts: xs[effIdx],
              median: med[effIdx],
              p25: u.data[2][effIdx] as number | null,
              p75: u.data[3][effIdx] as number | null,
              p2_5: u.data[4][effIdx] as number | null,
              p97_5: u.data[5][effIdx] as number | null,
              loss_pct: loss[effIdx]
            };
            tip.x = left;
            tip.y = top;
            tip.flipRight = u.bbox.width > 0 && left > u.bbox.width / 2;
            tip.flipDown = u.bbox.height > 0 && top > u.bbox.height / 2;
            tip.visible = true;
          }
        ],
        setSelect: [
          (u) => {
            if (!onZoom) return;
            const { left, width } = u.select;
            if (width < 5) return;
            const x0 = u.posToVal(left, 'x');
            const x1 = u.posToVal(left + width, 'x');
            onZoom(Math.round(x0), Math.round(x1));
            u.setSelect({ left: 0, top: 0, width: 0, height: 0 }, false);
          }
        ]
      }
    };
  }

  // Two effects, split so Svelte's reactivity graph fires the right one for
  // the right input. Combined with the drawers reading resolution lazily,
  // this means we never destroy + recreate the uPlot instance after the
  // initial mount - no blank canvas, no layout shift.
  //
  // Effect A (series): pack the data and call setData. uPlot handles
  // arbitrary-length arrays, so a zoom that returns a different bucket count
  // is just a setData call, not a teardown.
  //
  // Effect B (xMin/xMax): call setScale. Fires every 1 s in live mode; one
  // cheap redraw per tick, no canvas clear sequence.
  $effect(() => {
    if (!container) return;
    const data = pack(series);
    if (plot) {
      // Skip live-refresh redraws while the user is mid-touch — the
      // setData/setScale combo refits the visible window and ruins
      // in-flight zoom gestures. Once touchState flips back to 'idle'
      // the effect re-runs and applies the latest data.
      if (touchState !== 'idle') return;
      plot.setData(data, true);
      // setData(true) refits the x scale to the data; if the parent has
      // pinned a window we need to re-apply it right away so the right edge
      // doesn't snap to the latest sample for one frame.
      if (xMin != null && xMax != null) {
        plot.setScale('x', { min: xMin, max: xMax });
        lastXMin = xMin;
        lastXMax = xMax;
      }
      return;
    }
    const opts = makeOpts(container.clientWidth || 600);
    plot = new uPlot(opts, data, container);
    if (xMin != null && xMax != null) {
      plot.setScale('x', { min: xMin, max: xMax });
      lastXMin = xMin;
      lastXMax = xMax;
    }
    const ro = new ResizeObserver(() => {
      if (container && plot) plot.setSize({ width: container.clientWidth, height });
    });
    ro.observe(container);
    return () => ro.disconnect();
  });

  $effect(() => {
    const min = xMin;
    const max = xMax;
    if (!plot || min == null || max == null) return;
    if (min === lastXMin && max === lastXMax) return;
    // Same touchState gate — don't shift the scale under a finger.
    if (touchState !== 'idle') return;
    plot.setScale('x', { min, max });
    lastXMin = min;
    lastXMax = max;
  });

  onDestroy(() => {
    plot?.destroy();
    plot = null;
  });
</script>

<div class="w-full">
  <div
    bind:this={wrapper}
    class="w-full relative"
    style="height: {height}px; touch-action: pan-y; -webkit-user-select: none; user-select: none"
    oncontextmenu={onContextMenu}
    role="presentation"
  >
    <div bind:this={container} class="w-full h-full overflow-hidden"></div>
    <!-- Touch-mode drag-to-zoom selection band. uPlot's built-in drag is
         mouse-only, so on touch we paint our own translucent band from
         the press position to the current finger position and call
         onZoom on release. -->
    {#if touchDragRect}
      <div
        class="absolute pointer-events-none"
        style="left: {Math.min(touchDragRect.startX, touchDragRect.endX)}px; width: {Math.abs(touchDragRect.endX - touchDragRect.startX)}px; top: 0; bottom: 0; background: rgba(78, 161, 255, 0.18); border-left: 1px solid rgba(78, 161, 255, 0.55); border-right: 1px solid rgba(78, 161, 255, 0.55)"
      ></div>
    {/if}
    {#if tip.visible && tip.sample && !viewport.isMobile}
      {@const s = tip.sample}
      <div
        class="absolute pointer-events-none rounded shadow-md text-xs"
        style="border: 1px solid var(--border); background: var(--bg); color: var(--fg); padding: 6px 8px; min-width: 150px; {tip.flipDown ? `bottom: ${Math.max(0, (wrapper?.clientHeight ?? 0) - tip.y + 12)}px;` : `top: ${tip.y + 12}px;`} {tip.flipRight ? `right: ${Math.max(0, (wrapper?.clientWidth ?? 0) - tip.x + 12)}px;` : `left: ${tip.x + 12}px;`}"
      >
        <div class="font-medium mb-1 mono" style="color: var(--muted)">
          {formatTipTimestamp(s.ts)}
        </div>
        <div class="grid grid-cols-[auto_auto] gap-x-3 gap-y-0.5 mono">
          <span style="font-weight: 600; color: var(--smoke-outer-text)">p97.5</span>
          <span class="text-right" style="font-weight: 600; color: var(--smoke-outer-text)">{formatMs(s.p97_5)}</span>
          <span style="font-weight: 600; color: var(--smoke-inner-text)">p75</span>
          <span class="text-right" style="font-weight: 600; color: var(--smoke-inner-text)">{formatMs(s.p75)}</span>
          <span style="font-weight: 600">median</span>
          <span class="text-right" style="font-weight: 600; color: {lossColor(s.loss_pct)}">{formatMs(s.median)}</span>
          <span style="font-weight: 600; color: var(--smoke-inner-text)">p25</span>
          <span class="text-right" style="font-weight: 600; color: var(--smoke-inner-text)">{formatMs(s.p25)}</span>
          <span style="font-weight: 600; color: var(--smoke-outer-text)">p2.5</span>
          <span class="text-right" style="font-weight: 600; color: var(--smoke-outer-text)">{formatMs(s.p2_5)}</span>
          <span class="mt-0.5" style="font-weight: 600">loss</span>
          <span
            class="text-right mt-0.5"
            style="font-weight: 600; {s.loss_pct === 100 ? 'color: var(--latency-bad)' : ''}"
          >{formatLoss(s.loss_pct)}</span>
        </div>
      </div>
    {/if}
  </div>
  <div class="mono leading-snug px-1 pt-1" style="color: var(--muted); font-size: 11px">
    <div class="flex flex-wrap gap-x-3 gap-y-0">
      <span style="color: var(--fg); font-weight: 500">median rtt:</span>
      <span><span style="color: var(--fg)">{formatMs(medianStats?.avg ?? null)}</span> avg</span>
      <span><span style="color: var(--fg)">{formatMs(medianStats?.max ?? null)}</span> max</span>
      <span><span style="color: var(--fg)">{formatMs(medianStats?.min ?? null)}</span> min</span>
      <span><span style="color: var(--fg)">{formatMs(medianStats?.now ?? null)}</span> now</span>
      <span><span style="color: var(--fg)">{formatMs(medianStats?.sd ?? null)}</span> sd</span>
    </div>
    <div class="flex flex-wrap gap-x-3 gap-y-0">
      <span style="color: var(--fg); font-weight: 500">packet loss:</span>
      <span><span style="color: var(--fg)">{formatLoss(lossStats?.avg ?? null)}</span> avg</span>
      <span><span style="color: var(--fg)">{formatLoss(lossStats?.max ?? null)}</span> max</span>
      <span><span style="color: var(--fg)">{formatLoss(lossStats?.min ?? null)}</span> min</span>
      <span><span style="color: var(--fg)">{formatLoss(lossStats?.now ?? null)}</span> now</span>
    </div>
  </div>
</div>

<!-- Mobile tooltip: pinned to the bottom of the viewport so the finger
     never occludes the data point under inspection. Only renders after
     the long-press timer has fired (touchState leaves 'pending'/'drag').
     Stays visible until the next pointerdown.
     Layout: line 1 timestamp, line 2 p97.5 + p75, line 3 median + loss,
     line 4 p25 + p2.5. Each label-value pair uses a normal space so the
     label visually separates from the value. -->
{#if viewport.isMobile && tip.visible && tip.sample}
  {@const s = tip.sample}
  <div
    class="fixed bottom-0 left-0 right-0 z-30 mono pointer-events-none leading-tight"
    style="background: var(--bg); border-top: 1px solid var(--border); color: var(--fg); padding: 6px 10px env(safe-area-inset-bottom, 6px); font-size: 12px"
  >
    <div class="font-medium" style="color: var(--muted)">{formatTipTimestamp(s.ts)}</div>
    <div class="flex flex-wrap gap-x-5 mt-0.5">
      <span><span style="color: var(--smoke-outer-text)">p97.5</span> {formatMs(s.p97_5)}</span>
      <span><span style="color: var(--smoke-inner-text)">p75</span> {formatMs(s.p75)}</span>
    </div>
    <div class="flex flex-wrap gap-x-5">
      <span class="font-semibold"><span style="color: var(--muted); font-weight: 400">median</span> <span style="color: {lossColor(s.loss_pct)}">{formatMs(s.median)}</span></span>
      <span class="font-semibold"><span style="color: var(--muted); font-weight: 400">loss</span> <span style="{s.loss_pct === 100 ? 'color: var(--latency-bad)' : ''}">{formatLoss(s.loss_pct)}</span></span>
    </div>
    <div class="flex flex-wrap gap-x-5">
      <span><span style="color: var(--smoke-inner-text)">p25</span> {formatMs(s.p25)}</span>
      <span><span style="color: var(--smoke-outer-text)">p2.5</span> {formatMs(s.p2_5)}</span>
    </div>
  </div>
{/if}

<style>
  /* Override uPlot's default cursor crosshair: solid 1px light gray vertical
   * line, no horizontal line (disabled via cursor.y = false above). */
  :global(.u-cursor-x) {
    border-style: solid !important;
    border-width: 0 1px 0 0 !important;
    border-color: rgba(0, 0, 0, 0.28) !important;
  }
</style>
