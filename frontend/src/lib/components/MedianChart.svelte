<script lang="ts">
  import uPlot from 'uplot';
  import 'uplot/dist/uPlot.min.css';
  import { onMount, onDestroy } from 'svelte';
  import type { SeriesResp } from '$lib/api';

  let { series, height = 200 }: { series: SeriesResp; height?: number } = $props();

  let container: HTMLDivElement | undefined = $state();
  let plot: uPlot | null = null;

  function dataFor(s: SeriesResp): uPlot.AlignedData {
    const xs = s.samples.map((p) => p.ts);
    const ys = s.samples.map((p) => (p.median !== undefined ? p.median : null));
    return [xs, ys as (number | null)[]];
  }

  function makeOpts(width: number, _s: SeriesResp): uPlot.Options {
    return {
      width,
      height,
      legend: { show: false },
      scales: {
        x: { time: true },
        y: { auto: true }
      },
      series: [
        {},
        {
          stroke: getCssVar('--accent') || '#4ea1ff',
          width: 1.5,
          spanGaps: false,
          label: 'median (ms)'
        }
      ],
      axes: [
        { stroke: getCssVar('--muted') || '#6b7480' },
        {
          stroke: getCssVar('--muted') || '#6b7480',
          label: 'ms'
        }
      ],
      cursor: { drag: { x: true, y: false } }
    };
  }

  function getCssVar(name: string): string {
    if (typeof window === 'undefined') return '';
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }

  $effect(() => {
    if (!container) return;
    if (plot) {
      plot.setData(dataFor(series));
    } else {
      const opts = makeOpts(container.clientWidth, series);
      plot = new uPlot(opts, dataFor(series), container);
      const ro = new ResizeObserver(() => {
        if (container && plot) plot.setSize({ width: container.clientWidth, height });
      });
      ro.observe(container);
      return () => ro.disconnect();
    }
  });

  onDestroy(() => {
    plot?.destroy();
    plot = null;
  });
</script>

<div bind:this={container} class="w-full overflow-hidden" style="height: {height}px"></div>
