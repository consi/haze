import json, time, uuid, os, shutil
from urllib.parse import urlparse, parse_qs
from playwright.sync_api import sync_playwright
HOST='11111111-1111-4111-8111-111111111111'
now=int(time.time())
host={'uuid':HOST,'display_name':'Amsterdam · edge gateway','probe_type':'ping','probe_config':{'target':'203.0.113.40'},'interval_secs':30,'samples_per_period':20,'chunk_window_secs':3600,'enabled':True,'created_at':now-86400,'group_uuids':[]}
def trace(i,ts,old=False):
    ips=['192.168.10.1','10.42.0.1','198.51.100.14' if old else '198.51.100.22','203.0.113.40']
    names=['gateway.home','core-01.waw.example.net','transit-ams-01.example.net' if old else 'transit-ams-02.example.net','edge-amsterdam.example.net']
    return {'id':str(uuid.UUID(int=i+1)),'host_uuid':HOST,'sequence':i+1,'timestamp':ts,'kind':'trace','version':1,'context':{'target':'203.0.113.40','hops':[[{'ip':ip,'dns':dns}] for ip,dns in zip(ips,names)]},'data':{'event':'route_changed','started':ts-6,'finished':ts,'previous_observed':ts-300,'reached':True,'hops':[{'sent':5,'received':5 if j!=2 else 4,'avg_ms':v,'loss_pct':0 if j!=2 else 20} for j,v in enumerate([.6,2.4,18.9,23.7])]}}
records=[trace(i,now-120-i*300) for i in range(600)]
errors=[]
with sync_playwright() as p:
    browser=p.chromium.launch(executable_path=os.environ.get('CHROMIUM',shutil.which('chromium')),headless=True,args=['--no-sandbox'])
    page=browser.new_page(viewport={'width':1440,'height':1000},device_scale_factor=1)
    page.on('pageerror',lambda e:errors.append(str(e)))
    def route(req):
        u=urlparse(req.request.url); path=u.path.split('/api/v1')[-1];q=parse_qs(u.query)
        if path=='/auth/me':body={'id':1,'username':'reviewer','role':'admin'}
        elif path=='/server-info':body={'version':'0.6.0-dev.1','public_mode_enabled':True}
        elif path=='/tree':body={'groups':[],'hosts':[host]}
        elif path=='/settings/storage':body={'retention_tiers':[{'max_age_secs':31536000,'resolution_secs':0}],'compactor_interval_secs':3600}
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
    assert not errors,errors
    browser.close()
print('Browser checks passed: dark/light/mobile, DNS/IP, focus, Escape, timeline selection.')
