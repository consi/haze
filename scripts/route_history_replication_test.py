#!/usr/bin/env python3
"""Isolated native trace + replication/restart smoke test. No production DB access.
HAZE_TEST_BINARY selects the built binary. CAP_NET_RAW is required.
"""
import http.cookiejar,json,os,pathlib,secrets,subprocess,tempfile,time,urllib.request
binary=os.path.abspath(os.environ.get('HAZE_TEST_BINARY','target/release/haze'))
root=pathlib.Path(tempfile.mkdtemp(prefix='haze-route-smoke-'))
password=secrets.token_urlsafe(24)
processes={};clients={}
def start(n):
    log=open(root/f'{n}.log','ab')
    env={**os.environ,'HAZE_BOOTSTRAP_PASSWORD':password}
    processes[n]=subprocess.Popen([binary,'--data-dir',str(root/str(n)),'--bind',f'127.0.0.1:{4520+n}'],env=env,stdout=log,stderr=log)
    clients[n]=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
    wait(lambda: health(n),30)
    request(n,'POST','/auth/login',{'username':'admin','password':password})
def health(n):
    try:return urllib.request.urlopen(f'http://127.0.0.1:{4520+n}/healthz',timeout=2).status==200
    except Exception:return False
def request(n,method,path,body=None):
    req=urllib.request.Request(f'http://127.0.0.1:{4520+n}/api/v1'+path,data=json.dumps(body).encode() if body is not None else None,method=method,headers={'Content-Type':'application/json'})
    with clients[n].open(req,timeout=10) as response:
        data=response.read();return json.loads(data) if data else None
def wait(fn,seconds=90):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        try:
            result=fn()
            if result:return result
        except (urllib.error.URLError,KeyError):pass
        time.sleep(.5)
    raise AssertionError('Timed out; logs: '+str(root))
def history(n,host):return request(n,'GET',f'/hosts/{host}/route-history?from={int(time.time())-3600}&to={int(time.time())}&all=true')
def stop(n):
    processes[n].terminate()
    try:processes[n].wait(timeout=15)
    except subprocess.TimeoutExpired:processes[n].kill();processes[n].wait()
try:
    start(1);start(2)
    hosts=[]
    for address in ['127.0.0.1','::1']:
        host=request(1,'POST','/hosts',{'display_name':address,'probe_type':'ping','probe_config':{'target':address},'interval_secs':1,'samples_per_period':1})['uuid'];hosts.append(host)
        rows=wait(lambda:history(1,host)['records'])
        detail=request(1,'GET',f"/hosts/{host}/route-history/{rows[0]['id']}")['trace']
        assert detail['data']['reached'],detail['data']
        assert len(detail['context']['hops'])==1,detail
        assert detail['data']['hops'][0]['received']==5,detail
        print(address,'native trace passed',flush=True)
    token=request(1,'POST','/user/tokens',{'name':'isolated replication test','replication_only':True})['plaintext']
    peer=request(2,'POST','/replication/peers',{'name':'source','base_url':'http://127.0.0.1:4521','api_token':token,'reconcile_interval_secs':30})
    rule=request(2,'POST','/replication/rules',{'peer_uuid':peer['uuid']})
    for host in hosts:
        source=history(1,host)['records'];expected={r['id'] for r in source}
        wait(lambda:expected.issubset({r['id'] for r in history(2,host)['records']}))
    print('Metadata backfill preserved origin IDs',flush=True)
    count=len(history(2,hosts[0])['records'])
    wait(lambda:len(history(2,hosts[0])['records'])>count)
    print('Live metadata replication passed',flush=True)
    stop(2);time.sleep(12);start(2)
    expected={r['id'] for r in history(1,hosts[0])['records']}
    wait(lambda:expected.issubset({r['id'] for r in history(2,hosts[0])['records']}))
    received=history(2,hosts[0])['records'];assert len(received)==len({r['id'] for r in received})
    print('Restart/catch-up without duplicates passed',flush=True)
finally:
    for n,p in processes.items():
        if p.poll() is None:stop(n)
print('Isolated route-history smoke test passed; logs:',root)
