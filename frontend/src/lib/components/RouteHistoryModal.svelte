<script lang="ts">
  import {onMount,untrack} from 'svelte';
  import {api,type Host,type RouteRecord,type RouteHistory,type RouteDetail} from '$lib/api';
  import {currentTimeZone} from '$lib/timezone.svelte';
  import Modal from './Modal.svelte';
  let {host,fromSecs,toSecs,onClose}:{host:Host;fromSecs:number;toSecs:number;onClose:()=>void}=$props();
  let from=$state(untrack(()=>Math.max(0,Math.floor(fromSecs)))),to=$state(untrack(()=>Math.floor(toSecs)));
  let all=$state(false),dns=$state(true),loading=$state(false),detailLoading=$state(false);
  let error=$state(''),data=$state<RouteHistory|null>(null),selected=$state<RouteDetail|null>(null);
  let selectedId=$state(''),jump=$state(''),brushStart=$state<number|null>(null),brushEnd=$state<number|null>(null);
  let overview:SVGSVGElement;let list=$state<HTMLDivElement>();
  let controller:AbortController|undefined,detailController:AbortController|undefined;
  let generation=0;
  const presets=[['1h',3600],['6h',21600],['24h',86400],['7d',604800],['30d',2592000]] as const;
  const labels:Record<string,string>={baseline:'First observed route',route_changed:'Route changed',visibility_changed:'Hop visibility changed',trace_failed:'Trace failed',collection_gap:'Collection gap',incomplete:'Destination not reached',loss_started:'Packet loss started',loss_changed:'Packet loss changed',loss_recovered:'Packet loss recovered'};
  const fmt=(ts:number)=>new Date(ts*1000).toLocaleString(undefined,{timeZone:currentTimeZone(),month:'short',day:'numeric',hour:'2-digit',minute:'2-digit',second:'2-digit',hour12:false});
  const short=(ts:number)=>new Date(ts*1000).toLocaleTimeString(undefined,{timeZone:currentTimeZone(),hour:'2-digit',minute:'2-digit',hour12:false});
  const title=(r:RouteRecord)=>labels[r.data.event??'']??'Route sampled';
  const color=(r:RouteRecord)=>r.kind==='loss'?(r.data.loss_pct===0?'var(--latency-good)':'var(--latency-bad)'):r.data.event==='route_changed'?'var(--accent)':r.data.error?'var(--latency-warn)':'var(--muted)';
  let selectedIndex=$derived(data?.records.findIndex(r=>r.id===selectedId)??-1);
  let trace=$derived(selected?.trace);
  let previous=$derived(selected?.previous);
  let hops=$derived(trace?.context?.hops??[]);
  let oldHops=$derived(previous?.context?.hops??[]);
  let rowCount=$derived(Math.max(hops.length,oldHops.length));
  let changedCount=$derived(Array.from({length:rowCount},(_,i)=>i).filter(i=>different(i)).length);
  function different(i:number){return !!previous&&JSON.stringify((hops[i]??[]).map(a=>a.ip))!==JSON.stringify((oldHops[i]??[]).map(a=>a.ip));}
  async function load(append=false,at?:number,newer=false){
    if(append&&!(newer?data?.newer:data?.next))return;
    controller?.abort();controller=new AbortController();const current=++generation;
    loading=true;error='';
    try {
      const result=await api.routeHistory(host.uuid,from,to,all,append?(newer?data?.newer:data?.next):undefined,controller.signal,at,newer);
      if(current!==generation)return;
      data=append&&data?{...result,next:newer?data.next:result.next,newer:newer?result.newer:data.newer,records:newer?[...result.records,...data.records]:[...data.records,...result.records]}:result;
      // Keep DOM and memory bounded while retaining navigation in both directions.
      if(data.records.length>400){data.records=newer?data.records.slice(0,400):data.records.slice(-400);if(newer){const r=data.records.at(-1)!;data.next=`${r.timestamp}:${r.id}`;}else{const r=data.records[0];data.newer=`${r.timestamp}:${r.id}`;}}
      if(at!==undefined){const nearest=data.records.reduce<RouteRecord|null>((best,r)=>!best||Math.abs(r.timestamp-at)<Math.abs(best.timestamp-at)?r:best,null);if(nearest)void select(nearest);}
      if(at===undefined&&!append && !data.records.some(r=>r.id===selectedId)){
        selected=null;selectedId='';
        if(data.records.length)void select(data.records[0]);
      }
    }catch(e){if(e instanceof Error&&e.name!=='AbortError')error=e.message;}
    finally{if(current===generation)loading=false;}
  }
  async function select(r:RouteRecord){
    selectedId=r.id;detailController?.abort();detailController=new AbortController();detailLoading=true;
    try{const next=await api.routeDetail(host.uuid,r.id,detailController.signal);if(selectedId===r.id)selected=next;}
    catch(e){if(e instanceof Error&&e.name!=='AbortError')error=e.message;}
    finally{if(selectedId===r.id)detailLoading=false;}
  }
  async function step(direction:number){
    if(!data)return;
    const id=selectedId;
    if(direction<0&&selectedIndex===0&&data.newer)await load(true,undefined,true);
    if(direction>0&&selectedIndex===data.records.length-1&&data.next)await load(true);
    const i=data.records.findIndex(r=>r.id===id)+direction;
    if(data.records[i]){void select(data.records[i]);requestAnimationFrame(()=>list?.querySelector(`[data-record="${data?.records[i].id}"]`)?.scrollIntoView({block:'nearest'}));}
  }
  function range(seconds:number){to=Math.floor(Date.now()/1000);from=Math.max(0,to-seconds);void load();}
  function position(e:PointerEvent){const r=overview.getBoundingClientRect();return Math.max(0,Math.min(1000,(e.clientX-r.left)/r.width*1000));}
  function down(e:PointerEvent){if(e.button!==0)return;brushStart=position(e);brushEnd=brushStart;overview.setPointerCapture(e.pointerId);}
  function up(e:PointerEvent){
    if(brushStart===null)return;const end=position(e),start=brushStart;brushStart=null;brushEnd=null;
    if(Math.abs(end-start)>8){const span=to-from;const old=from;from=Math.floor(old+Math.min(start,end)/1000*span);to=Math.max(from+1,Math.floor(old+Math.max(start,end)/1000*span));void load();}
    else {const ts=from+end/1000*(to-from);void load(false,ts);}
  }
  function jumpTo(){const date=new Date(jump);if(!Number.isFinite(date.getTime()))return;const center=Math.floor(date.getTime()/1000);from=Math.max(0,center-1800);to=center+1800;void load(false,center);}
  function keys(e:KeyboardEvent){if((e.target as HTMLElement)?.matches('input,select,textarea'))return;if(e.key==='ArrowLeft'||e.key==='ArrowRight'){e.preventDefault();void step(e.key==='ArrowLeft'?1:-1);}}
  onMount(()=>{void load();return()=>{controller?.abort();detailController?.abort();};});
</script>
<svelte:window onkeydown={keys}/>
<Modal title={`Route history · ${host.display_name}`} {onClose} wide>
  <div class="route-history">
    <div class="flex flex-wrap items-center justify-between gap-2 mb-3">
      <div><div class="text-xs font-semibold">Paths over time</div><div class="text-[11px] muted mt-0.5">Periodic ICMP traces · 5 attempts per hop</div></div>
      <div class="flex gap-1 items-center">
        {#each presets as [label,seconds]}<button class:active={to-from===seconds} onclick={()=>range(seconds)}>{label}</button>{/each}
        <button onclick={()=>{from=Math.floor(fromSecs);to=Math.floor(toSecs);void load();}} title="Return to the graph's time range">Graph range</button>
        <button onclick={()=>void load()} disabled={loading} aria-label="Refresh route history">↻</button>
      </div>
    </div>
    <div class="timeline-panel rounded border p-2">
      <div class="flex justify-between items-center text-[10px] muted mb-2"><span>{fmt(from)}</span><span>Drag to zoom · click to inspect</span><span>{fmt(to)}</span></div>
      <!-- Pointer interaction supplements the event list and previous/next controls. -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <svg bind:this={overview} viewBox="0 0 1000 65" preserveAspectRatio="none" class="timeline" aria-label="Route changes and destination packet loss timeline" onpointerdown={down} onpointermove={e=>{if(brushStart!==null)brushEnd=position(e);}} onpointerup={up} onpointercancel={()=>{brushStart=null;brushEnd=null;}}>
        <line x1="0" y1="24" x2="1000" y2="24" stroke="var(--border)"/>
        <line x1="0" y1="61" x2="1000" y2="61" stroke="var(--border)"/>
        {#each data?.timeline??[] as b,i}
          {#if b.loss_pct>0}<rect x={i/240*1000} y={61-Math.max(4,b.loss_pct*.26)} width={1000/240+.2} height={Math.max(4,b.loss_pct*.26)} fill="var(--latency-bad)" opacity=".7"><title>{short(b.timestamp)} · destination loss up to {b.loss_pct.toFixed(0)}%</title></rect>{/if}
          {#if b.traces}<circle cx={(i+.5)/240*1000} cy="24" r="1.5" fill="var(--muted)"/>{/if}
          {#if b.changes}<path d={`M${(i+.5)/240*1000} 16l5 8-5 8-5-8z`} fill="var(--accent)"><title>{b.changes} observed route changes · {fmt(b.timestamp)}</title></path>{/if}
          {#if b.gaps}<line x1={(i+.5)/240*1000} x2={(i+.5)/240*1000} y1="12" y2="34" stroke="var(--latency-warn)" stroke-dasharray="2 2"/>{/if}
        {/each}
        {#if selected?.selected}<line x1={(selected.selected.timestamp-from)/(to-from)*1000} x2={(selected.selected.timestamp-from)/(to-from)*1000} y1="0" y2="65" stroke="var(--fg)" stroke-width="1"/>{/if}
        {#if brushStart!==null&&brushEnd!==null}<rect x={Math.min(brushStart,brushEnd)} y="0" width={Math.abs(brushEnd-brushStart)} height="65" fill="var(--accent)" opacity=".18"/>{/if}
      </svg>
      <div class="flex flex-wrap gap-4 text-[10px] mt-1 muted"><span><i style="background:var(--accent)"></i>Route change</span><span><i style="background:var(--latency-bad)"></i>Destination loss</span><span><i style="background:var(--latency-warn)"></i>Incomplete / gap</span><span class="ml-auto">{data?.total??0} {all?'observations':'events'}</span></div>
    </div>
    <div class="flex flex-wrap items-center gap-2 my-3">
      <div class="flex gap-1"><button class:active={!all} onclick={()=>{all=false;void load();}}>Changes & loss</button><button class:active={all} onclick={()=>{all=true;void load();}}>All traces</button></div>
      <div class="ml-auto flex items-center gap-1"><input type="datetime-local" bind:value={jump} aria-label="Jump to local date and time"/><button onclick={jumpTo} disabled={!jump}>Jump</button></div>
    </div>
    {#if error}<div class="error text-xs mb-2" role="alert">{error} <button onclick={()=>void load()}>Retry</button></div>{/if}
    {#if !data&&loading}<div class="empty muted">Loading route history…</div>
    {:else if !data?.records.length}<div class="empty"><div class="text-sm mb-2">{data?.support==='unsupported'?'Source does not support route history':'No route history in this range'}</div><p class="muted text-xs">{data?.support==='unsupported'?'Upgrade the source Haze instance to collect and replicate paths.':'History begins after upgrade, once ten ICMP graph samples have completed. Try a wider time range.'}</p></div>
    {:else}
      <div class="history-body">
        <div class="event-list" bind:this={list} aria-label="Route history events" aria-busy={loading}>
          {#if data.newer}<button class="w-full mb-2" onclick={()=>void load(true,undefined,true)} disabled={loading}>Load newer events</button>{/if}
          {#each data.records as r (r.id)}
            <button class="event-row" class:selected={selectedId===r.id} data-record={r.id} onclick={()=>void select(r)} aria-pressed={selectedId===r.id}>
              <span class="event-dot" style={`background:${color(r)}`}></span>
              <span class="min-w-0"><span class="block font-medium text-[11px]">{title(r)}</span><span class="block mono muted text-[10px] mt-1">{fmt(r.timestamp)}</span></span>
              {#if r.kind==='loss'}<span class="ml-auto mono text-[11px]" style={`color:${color(r)}`}>{r.data.loss_pct?.toFixed(0)}%</span>{/if}
            </button>
          {/each}
          {#if data.next}<button class="w-full mt-2" onclick={()=>void load(true)} disabled={loading}>{loading?'Loading…':'Load earlier events'}</button>{/if}
        </div>
        <div class="detail-panel min-w-0" aria-busy={detailLoading}>
          <div class="flex items-center justify-between gap-2 mb-3">
            <div class="text-xs font-semibold">{selected?title(selected.selected):'Select an event'}</div>
            <div class="flex gap-1"><button onclick={()=>void step(1)} disabled={selectedIndex>=data.records.length-1&&!data.next} title="Previous event (left arrow)">← Earlier</button><button onclick={()=>void step(-1)} disabled={selectedIndex<=0&&!data.newer} title="Next event (right arrow)">Later →</button></div>
          </div>
          {#if detailLoading}<p class="muted text-xs py-4">Loading path…</p>
          {:else if selected}
            <div class="text-[11px] muted mb-3">First observed <span class="mono" style="color:var(--fg)">{fmt(selected.selected.timestamp)}</span>
              {#if trace?.data.previous_observed}<span class="block mt-1">Compared with observation at {fmt(trace.data.previous_observed)}. The exact change time is between samples.</span>{/if}
            </div>
            {#if selected.selected.kind==='loss'}<div class="loss-callout rounded border p-2 mb-3 text-xs">Destination packet loss: <strong>{selected.selected.data.loss_pct?.toFixed(0)}%</strong><span class="block muted text-[11px] mt-1">{trace?`Most recent trace: ${fmt(trace.timestamp)}`:'No preceding traceroute is available.'}</span></div>{/if}
            {#if trace?.data.error}<div class="text-xs mb-3" style="color:var(--latency-warn)">{trace.data.error}</div>{/if}
            {#if trace?.context}
              <div class="flex flex-wrap items-center gap-2 mb-2"><span class="mono text-[11px] muted">{trace.context.target}</span><span class="text-[10px] muted">{hops.length} hops{previous?` · ${changedCount} changed`:''}</span><div class="ml-auto flex gap-1"><button class:active={dns} onclick={()=>dns=true}>DNS</button><button class:active={!dns} onclick={()=>dns=false}>IP</button></div></div>
              <div class="overflow-x-auto">
                <table class="hop-table"><thead><tr><th>#</th>{#if previous}<th>Previous path</th>{/if}<th>Observed path</th><th>RTT</th><th>Replies</th><th>Loss</th></tr></thead><tbody>
                  {#each Array.from({length:rowCount},(_,i)=>i) as i}
                    {@const metrics=trace.data.hops?.[i]}
                    <tr class:changed={different(i)}><td class="muted mono">{i+1}</td>
                      {#if previous}<td class="mono muted address">{#each oldHops[i]??[] as a}<span class="block" title={a.ip}>{dns?(a.dns||a.ip):a.ip}</span>{:else}<span>—</span>{/each}</td>{/if}
                      <td class="mono address">{#each hops[i]??[] as a}<span class="block" title={dns?a.ip:(a.dns||a.ip)}>{dns?(a.dns||a.ip):a.ip}</span>{:else}<span class="muted">{i<hops.length?'No reply':'—'}</span>{/each}</td>
                      <td class="mono whitespace-nowrap">{metrics?.avg_ms!=null?`${metrics.avg_ms.toFixed(1)} ms`:'—'}</td><td class="mono muted whitespace-nowrap">{metrics?`${metrics.received}/${metrics.sent}`:'—'}</td><td class="mono" style={`color:${metrics&&metrics.loss_pct>0?'var(--latency-warn)':'var(--muted)'}`}>{metrics?`${metrics.loss_pct.toFixed(0)}%`:'—'}</td></tr>
                  {/each}
                </tbody></table>
              </div>
              <p class="text-[10px] muted mt-3">Hop loss measures missing traceroute replies; routers may limit these while still forwarding traffic. Destination ping loss is shown separately above.</p>
              {#if !trace.data.reached}<p class="text-[11px] mt-2" style="color:var(--latency-warn)">Partial path · destination did not reply within the trace limit.</p>{/if}
            {/if}
          {/if}
        </div>
      </div>
    {/if}
  </div>
</Modal>
<style>
  .muted{color:var(--muted)}
  button{font-size:11px;padding:3px 7px;border-radius:3px;border:1px solid var(--border);color:var(--fg);background:var(--bg);white-space:nowrap;cursor:pointer}
  button:hover{border-color:var(--muted)}button:focus-visible,input:focus-visible{outline:2px solid var(--accent);outline-offset:2px}button:disabled{opacity:.4;cursor:default}
  button.active{background:var(--border);color:var(--accent);border-color:var(--accent)}
  input{font-size:11px;color:var(--fg);background:var(--bg);border:1px solid var(--border);padding:2px 5px;border-radius:3px;color-scheme:dark light}
  .timeline-panel,.loss-callout{border-color:var(--border)}.timeline{height:76px;width:100%;touch-action:none;cursor:crosshair;overflow:hidden}
  i{display:inline-block;width:6px;height:6px;border-radius:1px;margin-right:5px}
  .history-body{display:grid;grid-template-columns:245px minmax(0,1fr);border-top:1px solid var(--border)}
  .event-list{max-height:calc(100dvh - 300px);overflow-y:auto;padding:8px 8px 8px 0;border-right:1px solid var(--border)}
  button.event-row{display:flex;align-items:center;gap:8px;width:100%;text-align:left;padding:10px 7px;border-color:transparent;white-space:normal}
  button.event-row.selected{background:color-mix(in srgb,var(--accent) 9%,var(--bg));border-color:var(--border)}
  .event-dot{width:5px;height:5px;border-radius:50%;flex-shrink:0}.detail-panel{padding:14px 0 0 14px;max-height:calc(100dvh - 280px);overflow-y:auto}
  .hop-table{width:100%;text-align:left;font-size:10px;border-collapse:collapse}.hop-table th{font-weight:500;color:var(--muted);padding:6px 5px;white-space:nowrap;border-bottom:1px solid var(--border)}
  .hop-table td{padding:8px 5px;border-bottom:1px solid var(--border);vertical-align:top}.hop-table tr.changed{background:color-mix(in srgb,var(--accent) 7%,var(--bg))}.hop-table tr.changed td:first-child{box-shadow:inset 2px 0 var(--accent);color:var(--accent)}
  .address{overflow-wrap:anywhere;min-width:95px}.empty{text-align:center;padding:50px 12px}.error{color:var(--latency-bad)}
  @media(max-width:767px){.history-body{grid-template-columns:1fr}.event-list{max-height:180px;border-right:0;border-bottom:1px solid var(--border);padding-right:0}.detail-panel{padding-left:0;max-height:none}button{min-height:32px}.timeline-panel>div:first-child{font-size:9px}.timeline-panel>div:first-child>span:nth-child(2){display:none}input{max-width:200px}}
</style>
