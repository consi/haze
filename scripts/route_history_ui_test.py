import json, time, uuid, os, shutil
from urllib.parse import urlparse, parse_qs
from playwright.sync_api import sync_playwright
HOST='11111111-1111-4111-8111-111111111111'
now=int(time.time())
GROUP='22222222-2222-4222-8222-222222222222'
group={'uuid':GROUP,'display_name':'Transit group','parent_uuid':None,'path':'/'+GROUP+'/','depth':0,'created_at':now}
peer={'uuid':'33333333-3333-4333-8333-333333333333','name':'Remote transit','base_url':'https://'+'long-source-name-'*6+'.example.net','source_instance_uuid':GROUP,'upstream_chain':['44444444-4444-4444-8444-444444444444'],'tls_skip_verify':False,'reconcile_interval_secs':300,'created_at':now,'last_contact_at':now,'last_error':None,'source_version':'0.6.0','last_latency_ms':12}
role='admin'
host={'uuid':HOST,'display_name':'Amsterdam · edge gateway','probe_type':'ping','probe_config':{'target':'203.0.113.40'},'interval_secs':30,'samples_per_period':20,'chunk_window_secs':3600,'enabled':True,'created_at':now-86400,'group_uuids':[]}
def trace(i,ts,old=False):
    ips=['192.168.10.1','10.42.0.1','198.51.100.14' if old else '198.51.100.22','203.0.113.40']
    names=['gateway.home','core-01.waw.example.net','transit-ams-01.example.net' if old else 'transit-ams-02.example.net','edge-amsterdam.example.net']
    ips += ['2001:db8:abcd:1234:5678:90ab:cdef:1234'] * 28
    names += ['transit-' + 'long-router-name-' * 8 + '.example.net'] * 28
    return {'id':str(uuid.UUID(int=i+1)),'host_uuid':HOST,'sequence':i+1,'timestamp':ts,'kind':'trace','version':1,'context':{'target':'203.0.113.40','hops':[[{'ip':ip,'dns':dns}] for ip,dns in zip(ips,names)]},'data':{'event':'route_changed','started':ts-6,'finished':ts,'previous_observed':ts-300,'reached':True,'hops':[{'sent':5,'received':5 if j!=2 else 4,'avg_ms':v,'loss_pct':0 if j!=2 else 20} for j,v in enumerate([.6,2.4,18.9,23.7]+[24.0]*28)]}}
records=[trace(i,now-120-i*300) for i in range(600)]
errors=[]
with sync_playwright() as p:
    browser=p.chromium.launch(executable_path=os.environ.get('CHROMIUM',shutil.which('chromium')),headless=True,args=['--no-sandbox'])
    page=browser.new_page(viewport={'width':1440,'height':1000},device_scale_factor=1)
    page.on('pageerror',lambda e:errors.append(str(e)))
    def route(req):
        u=urlparse(req.request.url); path=u.path.split('/api/v1')[-1];q=parse_qs(u.query)
        if path=='/auth/me':body={'id':1,'username':'reviewer','role':role}
        elif path=='/server-info':body={'version':'0.6.0-dev.1','public_mode_enabled':True}
        elif path=='/tree':body={'groups':[group],'hosts':[host]}
        elif path=='/settings/storage':body={'retention_tiers':[{'max_age_secs':31536000,'resolution_secs':0}],'compactor_interval_secs':3600}
        elif path=='/groups':body=[group]
        elif path==f'/groups/{GROUP}':body=group
        elif path=='/replication/peers':body=[peer]
        elif path=='/hosts':body=[host]
        elif path=='/settings/workers':body={'pools':dict(probe_ping=4096,probe_traceroute=8,trace_every_entries=30,trace_queue_timeout_secs=300,trace_timeout_secs=60,trace_reply_timeout_ms=2000,probe_dns=1024,probe_tcp_connect=1024,probe_tls_connect=512,probe_http_ttfb=512,probe_http_total=512,compactor=8,alert_eval=32,replication=16)}
        elif path=='/settings/alerting':body={'settings':dict(eval_interval_secs=60,webhook_timeout_secs=10,snapshot_flush_interval_secs=300,min_window_secs=30,max_window_secs=604800)}
        elif path=='/settings/hosts':body={'defaults':dict(interval_secs=60,samples_per_period=20)}
        elif path=='/settings/public':body={'settings':dict(enabled=True,rate_limit_enabled=True,requests_per_second=20,burst=40)}
        elif path==f'/hosts/{HOST}':body=host
        elif path.endswith('/series'):
            fr=int(q.get('from',[now-86400])[0]);to=int(q.get('to',[now])[0]);body={'host_uuid':HOST,'from':fr,'to':to,'resolution_secs':300,'samples':[{'ts':fr+i*(to-fr)//100,'min':15,'p2_5':17,'p25':20,'median':23,'p75':25,'p97_5':29,'max':35,'loss_pct':0} for i in range(100)]}
        elif '/route-history/' in path:
            ident=path.rsplit('/',1)[-1];r=next(r for r in records if r['id']==ident);body={'selected':r,'trace':r,'previous':trace(9999,r['timestamp']-300,True)}
        elif path.endswith('/route-history'):
            fr=int(q['from'][0]);to=int(q['to'][0]);rs=[r for r in records if fr<=r['timestamp']<=to];offset=0
            if 'at' in q and rs:offset=max(0,min(range(len(rs)),key=lambda i:abs(rs[i]['timestamp']-int(q['at'][0])))-50)
            if 'before' in q:
                ts=int(q['before'][0].split(':')[0]);offset=next((i for i,r in enumerate(rs) if r['timestamp']<ts),len(rs))
            chunk=rs[offset:offset+100]
            body={'records':chunk,'next':f"{chunk[-1]['timestamp']}:{chunk[-1]['id']}" if offset+100<len(rs) else None,'newer':f"{chunk[0]['timestamp']}:{chunk[0]['id']}" if offset>0 and chunk else None,'total':len(rs),'support':'local','timeline':[{'timestamp':fr+i*(to-fr)//240,'traces':1,'changes':1 if i%27==0 else 0,'gaps':1 if i==70 else 0,'loss_pct':35 if 180<i<193 else 0} for i in range(240)]}
        elif path=='/events':req.fulfill(status=200,content_type='text/event-stream',body=': connected\n\n');return
        else:body=[]
        req.fulfill(status=200,content_type='application/json',body=json.dumps(body))
    page.route('**/api/v1/**',route)
    page.goto(os.environ.get('HAZE_TEST_URL','http://127.0.0.1:4421')+f'/hosts/{HOST}')
    page.wait_for_timeout(1000)
    page.get_by_role('button',name='Route history for Amsterdam').click()
    page.get_by_role('dialog').wait_for()
    box=page.get_by_role('dialog').bounding_box()
    assert box['width'] >= 1400 and box['height'] >= 970, box
    assert page.get_by_role('button',name='Route history for Amsterdam').inner_text().strip() == ''
    page.get_by_text('Observed path',exact=True).wait_for()
    assert page.get_by_role('dialog').evaluate('(e)=>e.contains(document.activeElement)')
    page.locator('html').evaluate('(e)=>e.dataset.theme="dark"')
    page.screenshot(path='/tmp/haze-route-dark.png',full_page=True)
    page.get_by_role('button',name='IP',exact=True).click()
    assert page.get_by_role('dialog').get_by_text('198.51.100.22',exact=True).count()>0
    page.get_by_role('button',name='DNS',exact=True).click()
    page.locator('html').evaluate('(e)=>e.dataset.theme="light"')
    page.screenshot(path='/tmp/haze-route-light.png',full_page=True)
    box=page.locator('svg.timeline').bounding_box();page.mouse.click(box['x']+box['width']*.15,box['y']+24)
    page.wait_for_timeout(200)
    page.keyboard.press('Escape');assert page.get_by_role('dialog').count()==0
    page.set_viewport_size({'width':390,'height':844})
    page.get_by_role('button',name='Route history for Amsterdam').click();page.get_by_text('Observed path',exact=True).wait_for()
    page.screenshot(path='/tmp/haze-route-mobile.png',full_page=True)
    assert page.get_by_role('dialog').bounding_box()['width']<=390
    page.keyboard.press('Escape')
    for width,height in [(320,640),(390,844),(768,600),(1024,600),(1440,1000),(844,390)]:
        page.set_viewport_size({'width':width,'height':height})
        page.get_by_role('button',name='Route history for Amsterdam').click()
        page.get_by_text('Observed path',exact=True).wait_for()
        dialog=page.get_by_role('dialog')
        assert dialog.evaluate('(e)=>e.scrollWidth<=e.clientWidth+1'), (width,'dialog overflow')
        assert page.locator('svg.timeline').bounding_box()['height']==32
        close=dialog.get_by_role('button',name='Close',exact=True)
        box=close.bounding_box()
        assert box['width']>= (44 if width<768 else 32)
        assert box['height']>= (44 if width<768 else 32)
        if width>=768:
            assert page.locator('.modal-body').evaluate('(e)=>e.scrollHeight<=e.clientHeight+1'), (width,'outer vertical overflow')
            assert page.locator('.detail-panel').evaluate('(e)=>e.scrollHeight>e.clientHeight'), (width,'fixture should overflow details')
        close.focus();page.keyboard.press('Tab')
        assert dialog.evaluate('(e)=>e.contains(document.activeElement)'), 'focus escaped'
        # The transparent corner of the button must be clickable too.
        close.click(position={'x':2,'y':2})
        assert page.get_by_role('dialog').count()==0
        assert page.get_by_role('button',name='Route history for Amsterdam').evaluate('(e)=>e===document.activeElement')
    records[0]['data'].update(event='incomplete',reached=False,error='Trace deadline exceeded; partial observations retained')
    records[0]['context']['hops'][0][0]['dns']=None
    page.get_by_role('button',name='Route history for Amsterdam').click()
    page.get_by_text('Partial path · destination did not reply.',exact=False).wait_for()
    assert page.get_by_role('dialog').get_by_text('192.168.10.1',exact=True).count()>0
    page.screenshot(path='/tmp/haze-route-partial.png',full_page=True)
    page.keyboard.press('Escape')
    # Optional axe-core installation lives outside application dependencies.
    axe=os.environ.get('HAZE_AXE_PATH')
    findings=[]
    def audit(label):
        print("Checking",label,flush=True)
        assert page.locator('html').evaluate('(e)=>e.scrollWidth<=e.clientWidth+1'), (label,'page overflow')
        assert page.locator('main').evaluate('(e)=>e.scrollWidth<=e.clientWidth+1'), (label,'main overflow')
        if axe:
            page.add_script_tag(path=axe)
            page.locator("html").evaluate("(e)=>e.dataset.theme='"+os.environ.get("HAZE_TEST_THEME","light")+"'")
            result=page.evaluate("async()=>await axe.run({runOnly:{type:'tag',values:['wcag2a','wcag2aa','wcag21aa','wcag22aa']}})")
            for v in result['violations']:
                findings.append({'page':label,'id':v['id'],'impact':v['impact'],'nodes':[{'target':n['target'],'summary':n['failureSummary']} for n in v['nodes']]})
    base=os.environ.get('HAZE_TEST_URL','http://127.0.0.1:4421')
    for width in map(int,os.environ.get("HAZE_UI_WIDTHS","320,390,768,1024,1440").split(",")):
        page.set_viewport_size({'width':width,'height':844})
        for route_name in ['/','/settings','/alerting','/user','/login',f'/groups/{GROUP}',f'/hosts/{HOST}']:
            page.goto(base+route_name);page.wait_for_timeout(350)
            audit(f'{route_name}@{width}')
        page.goto(base+'/');page.wait_for_timeout(250)
        for control in ['Add Host','Add Group','Edit Amsterdam · edge gateway','Edit Transit group']:
            if width<768:page.get_by_role('button',name='Open menu',exact=True).click()
            page.get_by_role('button',name=control,exact=True).click()
            dialog=page.get_by_role('dialog');dialog.wait_for()
            assert dialog.evaluate('(e)=>e.scrollWidth<=e.clientWidth+1'), (control,width,'overflow')
            audit(f'{control}@{width}')
            page.keyboard.press('Escape')
            if width<768 and page.get_by_role('button',name='Close menu',exact=True).count():page.get_by_role('button',name='Close menu',exact=True).click()
        page.goto(base+'/settings');page.wait_for_timeout(250)
        for control in ['+ Add peer','+ Add rule','Topology','Edit']:
            button=page.get_by_role('button',name=control,exact=True)
            if button.count() and button.is_enabled():
                button.click();page.get_by_role('dialog').wait_for()
                audit(f'{control}@{width}')
                if control=='Topology':
                    viewport=page.get_by_role('group',name='Topology viewport; arrow keys pan, Home resets')
                    viewport.focus();page.keyboard.press('ArrowDown');page.keyboard.press('Home')
                    page.keyboard.press('Tab')
                    assert page.get_by_role('dialog').locator('summary').evaluate('(e)=>e===document.activeElement')
                    page.get_by_role('button',name='Zoom in',exact=True).click()
                    page.get_by_text('Connections as text',exact=True).click()
                    assert 'Connections as text' in page.get_by_role('dialog').aria_snapshot()
                page.keyboard.press('Escape')
        page.goto(base+'/alerting');page.wait_for_timeout(250)
        page.get_by_role('button',name='+ New alert',exact=True).click()
        page.get_by_role('dialog').wait_for();audit(f'New alert@{width}');page.keyboard.press('Escape')
    # 1440x900 physical pixels at 200% zoom: 720x450 CSS pixels and DPR 2.
    zoom_context=browser.new_context(viewport={'width':720,'height':450},device_scale_factor=2)
    zoom_page=zoom_context.new_page();zoom_page.route('**/api/v1/**',route)
    zoom_page.on('pageerror',lambda e:errors.append(str(e)))
    zoom_page.goto(base+f'/hosts/{HOST}')
    zoom_page.get_by_role('button',name='Route history for Amsterdam').click()
    zoom_page.get_by_text('Observed path',exact=True).wait_for()
    assert zoom_page.get_by_role('dialog').bounding_box()['width']<=720
    assert zoom_page.get_by_role('dialog').evaluate('(e)=>e.scrollWidth<=e.clientWidth+1')
    zoom_page.screenshot(path='/tmp/haze-route-200pct.png')
    zoom_page.keyboard.press('Escape');zoom_context.close()
    role='viewer'
    page.goto(base+'/settings');page.wait_for_timeout(250)
    assert page.get_by_role('button',name='+ Add peer',exact=True).count()==0
    audit('viewer settings access')
    if axe:
        from pathlib import Path
        Path('/tmp/haze-accessibility-findings.json').write_text(json.dumps(findings,indent=2))
        assert not findings, f'{len(findings)} accessibility findings; see /tmp/haze-accessibility-findings.json'
    assert not errors,errors
    browser.close()
print('Browser checks passed: dark/light/mobile, DNS/IP, focus, Escape, timeline selection.')
