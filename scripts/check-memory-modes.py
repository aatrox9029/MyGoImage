"""Exercise default/high-resolution/unlimited and 24-image modes, recording RAM and UI proxies."""
import argparse,ctypes,ctypes.wintypes as wt,json,os,pathlib,subprocess,time,psutil,math,hashlib,datetime
root=pathlib.Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--output',type=pathlib.Path,default=root/'test-output'/('memory-modes-'+str(time.time_ns())))
parser.add_argument('--exe',type=pathlib.Path,default=root/'target/release/mygoimage.exe')
parser.add_argument('--mode',choices=['all','default','24mp','50mp','100mp','unlimited','multi24'],default='all')
parser.add_argument('--duration',type=float,default=4,help='Seconds per cold/warm run (default: 4)')
parser.add_argument('--bounce',action='store_true',help='Alternate multi-view page direction instead of stopping at the end')
a=parser.parse_args();out=a.output;out.mkdir(parents=True,exist_ok=True)
assert a.duration >= 4, 'Use at least four seconds per run'
photos=root/'test-output/photo-before/photos';assert photos.is_dir()
u=ctypes.WinDLL('user32');u.GetWindowThreadProcessId.argtypes=[wt.HWND,ctypes.POINTER(wt.DWORD)];u.IsWindowVisible.argtypes=[wt.HWND];u.PostMessageW.argtypes=[wt.HWND,wt.UINT,wt.WPARAM,wt.LPARAM]
u.GetWindowTextW.argtypes=[wt.HWND,wt.LPWSTR,ctypes.c_int]
CB=ctypes.WINFUNCTYPE(wt.BOOL,wt.HWND,wt.LPARAM);u.EnumWindows.argtypes=[CB,wt.LPARAM]
def find(pid):
    found=[]
    @CB
    def callback(hwnd,_):
        owner=wt.DWORD();u.GetWindowThreadProcessId(hwnd,ctypes.byref(owner))
        title=ctypes.create_unicode_buffer(256);u.GetWindowTextW(hwnd,title,len(title))
        if owner.value==pid and u.IsWindowVisible(hwnd) and title.value=='MyGoImage':found.append(hwnd)
        return True
    u.EnumWindows(callback,0);return found[0] if found else None
runs=[];invocation=str(time.time_ns())
for mode,preset,multi in [('default',0,False),('24mp',1,False),('50mp',2,False),('100mp',3,False),('unlimited',4,False),('multi24',0,True)]:
    if a.mode != 'all' and mode != a.mode:continue
    base=out/f'{invocation}-{mode}';data=base/'MyGoImage';data.mkdir(parents=True)
    (data/'settings.json').write_text(json.dumps(dict(image_size_limit=preset,multi_view=multi,images_per_view=24)),encoding='utf-8')
    for cache in ['cold','warm']:
        env=os.environ.copy();env['LOCALAPPDATA']=str(base.resolve())
        with (out/f'{mode}-{cache}.stderr.log').open('w') as log:
            started_utc_ms=time.time_ns()//1_000_000
            p=subprocess.Popen([str(a.exe.resolve()),str(photos)],env=env,stdout=log,stderr=log)
            proc=psutil.Process(p.pid);peak=0;cpu=0;reads=0;hwnd=None;start=time.perf_counter();last_key=0;rss_samples=[];moves=0
            try:
                while time.perf_counter()-start<a.duration:
                    assert p.poll() is None,(mode,p.returncode)
                    try:
                        rss=proc.memory_info().rss;peak=max(peak,rss);c=proc.cpu_times();cpu=c.user+c.system;reads=proc.io_counters().read_bytes
                        if not rss_samples or time.perf_counter()-start-rss_samples[-1]['seconds']>=.25:
                            rss_samples.append(dict(seconds=time.perf_counter()-start,rss_bytes=rss))
                    except psutil.Error:break
                    if hwnd is None:hwnd=find(p.pid)
                    if hwnd and time.perf_counter()-start>1 and time.perf_counter()-last_key>.15:
                        key=(0x26 if a.bounce and moves%2 else 0x28) if multi else 0x27;u.PostMessageW(hwnd,0x100,key,0);u.PostMessageW(hwnd,0x101,key,0);moves+=1
                        if not multi:u.PostMessageW(hwnd,0x100,ord('1'),0);u.PostMessageW(hwnd,0x101,ord('1'),0)
                        last_key=time.perf_counter()
                    time.sleep(.01)
            finally:
                if hwnd:u.PostMessageW(hwnd,0x10,0,0)
                try:p.wait(5)
                except subprocess.TimeoutExpired:p.kill();p.wait();raise
        records=[]
        for path in [data/'diagnostics.previous.jsonl',data/'diagnostics.jsonl']:
            if path.exists():records.extend(json.loads(line) for line in path.read_text().splitlines() if line)
        records=[r for r in records if r['utc_ms']>=started_utc_ms]
        timers=sorted(r['duration_us']/1000 for r in records if r['event']=='ui-timer-interval')
        pumps=sorted(r['duration_us']/1000 for r in records if r['event']=='ui-event-pump')
        memory={name:max((r['value'] for r in records if r['event']==name),default=0) for name in ['ram-cache-bytes','decoded-result-bytes','decoded-unique-bytes','ui-pixel-buffer-bytes']}
        assert memory['ui-pixel-buffer-bytes']>0,(mode,'no image uploaded')
        result=dict(mode=mode,preset=preset,cache=cache,duration_seconds=a.duration,bounce=a.bounce,navigation_commands=moves,peak_rss_bytes=peak,rss_samples=rss_samples,cpu_seconds=cpu,read_bytes=reads,gpu_memory_bytes=None,ui_timer_interval_p95_ms=timers[math.ceil((len(timers)-1)*.95)],ui_event_pump_p95_ms=pumps[math.ceil((len(pumps)-1)*.95)],max_accounted_bytes=memory,dropped_diagnostics=max((r['dropped_total'] for r in records),default=0))
        runs.append(result);print(json.dumps(result),flush=True)
(out/'results.json').write_text(json.dumps(dict(date=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(),exe_sha256=hashlib.sha256(a.exe.read_bytes()).hexdigest(),scope='48 NASA photo derivatives at 1800x1200; one cold and one warm smoke per mode; not a three-run performance acceptance, high-pixel stress or GPU/frame-time measurement; cache/result/UI figures overlap shared Arc buffers and exclude GPU/codec internal allocations',runs=runs),indent=2),encoding='utf-8')
