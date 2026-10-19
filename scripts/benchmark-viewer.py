"""Native presented-pixel benchmark. Generated fixtures are not a real-photo/SD-card claim."""
import argparse, ctypes, ctypes.wintypes as wt, datetime, hashlib, json, math, os
from pathlib import Path
import platform, shutil, subprocess, threading, time
import psutil
from PIL import Image, ImageDraw, ImageGrab, ImageOps

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--exe',type=Path,default=ROOT/'target/release/mygoimage.exe')
parser.add_argument('--output',type=Path,default=ROOT/'test-output/viewer-benchmark')
parser.add_argument('--storage',default='local NVMe; model recorded in docs/VALIDATION.md')
parser.add_argument('--runs',type=int,default=3)
parser.add_argument('--source-photo',type=Path)
invocation=str(time.time_ns())
a=parser.parse_args();a.output.mkdir(parents=True,exist_ok=True)
photos=a.output/'photos';photos.mkdir(exist_ok=True)
colors=[]
for i in range(48):
    color=(35+i*4,65+i*2,100+i*2);colors.append(color)
    ext=['jpg','png','webp','tiff'][i%4]
    path=photos/f'{i:03}.{ext}'
    if not path.exists():
        image=ImageOps.fit(Image.open(a.source_photo).convert('RGB'),(1800,1200)) if a.source_photo else Image.new('RGB',(1800,1200),color)
        if a.source_photo:ImageDraw.Draw(image).rectangle((900,350,1500,900),fill=color)
        else:ImageDraw.Draw(image).rectangle((0,0,300,1200),fill=(220,45,30))
        image.save(path)
manifest=[dict(name=p.name,bytes=p.stat().st_size,dimensions=Image.open(p).size,sha256=hashlib.sha256(p.read_bytes()).hexdigest()) for p in sorted(photos.iterdir())]
u=ctypes.WinDLL('user32',use_last_error=True);u.SetProcessDPIAware()
u.GetWindowThreadProcessId.argtypes=[wt.HWND,ctypes.POINTER(wt.DWORD)]
u.IsWindowVisible.argtypes=[wt.HWND]
u.GetWindowTextW.argtypes=[wt.HWND,wt.LPWSTR,ctypes.c_int]
u.PostMessageW.argtypes=[wt.HWND,wt.UINT,wt.WPARAM,wt.LPARAM]
u.ClientToScreen.argtypes=[wt.HWND,ctypes.POINTER(wt.POINT)]
u.SetWindowPos.argtypes=[wt.HWND,wt.HWND,ctypes.c_int,ctypes.c_int,ctypes.c_int,ctypes.c_int,wt.UINT]
CB=ctypes.WINFUNCTYPE(wt.BOOL,wt.HWND,wt.LPARAM);u.EnumWindows.argtypes=[CB,wt.LPARAM]
def window(pid):
    found=[]
    @CB
    def callback(hwnd,_):
        owner=wt.DWORD();u.GetWindowThreadProcessId(hwnd,ctypes.byref(owner))
        title=ctypes.create_unicode_buffer(256);u.GetWindowTextW(hwnd,title,len(title))
        if owner.value==pid and title.value=='MyGoImage' and u.IsWindowVisible(hwnd):found.append(hwnd)
        return True
    u.EnumWindows(callback,0);return found[0] if found else None

def percentiles(values):
    if not values:return None
    values=sorted(values)
    return {f'p{int(q*100)}':values[math.ceil((len(values)-1)*q)] for q in [.5,.95,.99]}

runs=[]
for run in range(a.runs):
    base=a.output/f'appdata-{invocation}-{run}'
    for cache in ['cold-application-cache','warm-application-cache']:
        env=os.environ.copy();env['LOCALAPPDATA']=str(base.resolve())
        stderr=a.output/f'{run}-{cache}.stderr.log'
        spawn_utc_us=time.time_ns()//1000
        start=time.perf_counter();p=subprocess.Popen([str(a.exe.resolve()),str(photos.resolve())],env=env,stdout=stderr.open('w'),stderr=subprocess.STDOUT)
        popen_ms=(time.perf_counter()-start)*1000
        process=psutil.Process(p.pid);sample={'peak_rss_bytes':0,'cpu_seconds':0,'read_bytes':0};stop=threading.Event()
        def monitor():
            while not stop.wait(.01):
                try:
                    sample['peak_rss_bytes']=max(sample['peak_rss_bytes'],process.memory_info().rss)
                    cpu=process.cpu_times();sample['cpu_seconds']=cpu.user+cpu.system
                    sample['read_bytes']=process.io_counters().read_bytes
                except psutil.Error:break
        watcher=threading.Thread(target=monitor);watcher.start()
        raw=[]
        try:
            deadline=time.perf_counter()+30;hwnd=None
            while not hwnd and time.perf_counter()<deadline:
                assert p.poll() is None,stderr
                hwnd=window(p.pid);time.sleep(.01)
            assert hwnd,'window timeout'
            window_found_ms=(time.perf_counter()-start)*1000
            u.SetWindowPos(hwnd,wt.HWND(-1),30,30,1280,800,0x40)
            point=wt.POINT(900,420);u.ClientToScreen(hwnd,ctypes.byref(point))
            def wait_color(index,sent):
                deadline=time.perf_counter()+15
                while time.perf_counter()<deadline:
                    color=ImageGrab.grab(bbox=(point.x,point.y,point.x+1,point.y+1)).getpixel((0,0))[:3]
                    if max(abs(x-y) for x,y in zip(color,colors[index]))<=5:
                        return (time.perf_counter()-sent)*1000
                    assert p.poll() is None,stderr
                    time.sleep(.005)
                raise AssertionError(f'pixel timeout: index={index}, observed={color}')
            raw.append(dict(scenario='startup-first-presented-pixel',ms=wait_color(0,start)))
            time.sleep(1)
            for index in [1,2,3,2,1,0,1,2]:
                key=0x27 if index>int(raw[-1].get('index',0)) else 0x25
                sent=time.perf_counter();u.PostMessageW(hwnd,0x100,key,0);u.PostMessageW(hwnd,0x101,key,0)
                raw.append(dict(scenario='adjacent-presented-pixel',index=index,ms=wait_color(index,sent)))
                time.sleep(.15)
            sent=time.perf_counter();u.PostMessageW(hwnd,0x100,0x23,0);u.PostMessageW(hwnd,0x101,0x23,0)
            raw.append(dict(scenario='end-presented-pixel',index=47,ms=wait_color(47,sent)))
            ImageGrab.grab().crop((30,30,1310,830)).save(a.output/f'{run}-{cache}.png')
        finally:
            if p.poll() is None:
                if hwnd:u.PostMessageW(hwnd,0x10,0,0)
                try:p.wait(5)
                except subprocess.TimeoutExpired:p.kill();p.wait()
            stop.set();watcher.join()
        records=[]
        for diagnostic in [base/'MyGoImage'/'diagnostics.previous.jsonl',base/'MyGoImage'/'diagnostics.jsonl']:
            if diagnostic.exists():
                records.extend(json.loads(line) for line in diagnostic.read_text(encoding='utf-8').splitlines() if line.strip())
        # Warm restarts append to the same files. Keep only this process session.
        marker='startup-main-to-telemetry' if any(r['event']=='startup-main-to-telemetry' for r in records) else 'render-notifier-supported'
        boundaries=[i for i,r in enumerate(records) if r['event']==marker]
        if boundaries:records=records[boundaries[-1]:]
        entered=next((r['value'] for r in records if r['event']=='startup-main-entry-utc-us'),None)
        entry_ms=(entered-spawn_utc_us)/1000 if entered is not None and entered>=spawn_utc_us else None
        startup_timing=dict(popen_ms=popen_ms,window_found_ms=window_found_ms,
                            spawn_to_main_wall_clock_ms=entry_ms,
                            clock_note='perf_counter for durations; UTC wall clock for cross-process main entry, not an additive exclusive bucket')
        trace_path=a.output/f'{run}-{cache}.diagnostics.json'
        trace_path.write_text(json.dumps(records,indent=2),encoding='utf-8')
        stage_events=['image-io','image-header','image-codec','image-orientation','image-resize','image-rgba','image-icc','image-other','image-total','ui-main-buffer-apply','ui-apply-to-after-render','request-to-after-render','render-callback']
        diagnostic_summary={event:percentiles([r['duration_us']/1000 for r in records if r['event']==event and r['ok']]) for event in stage_events}
        runs.append(dict(run=run+1,cache=cache,**sample,samples=raw,startup_timing=startup_timing,diagnostic_summary_ms=diagnostic_summary,diagnostic_record_count=len(records),diagnostic_dropped_total=max((r['dropped_total'] for r in records),default=0),render_notifier_supported=any(r['event']=='render-notifier-supported' and r['ok'] for r in records)))
        print(json.dumps(runs[-1]),flush=True)
summary={scenario:percentiles([s['ms'] for r in runs for s in r['samples'] if s['scenario']==scenario]) for scenario in ['startup-first-presented-pixel','adjacent-presented-pixel','end-presented-pixel']}
report=dict(date=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(),exe_sha256=hashlib.sha256(a.exe.read_bytes()).hexdigest(),platform=platform.platform(),cpu=platform.processor(),ram_bytes=psutil.virtual_memory().total,storage=a.storage,os_cache_flushed=False,scope=('NASA photograph derivatives with visible color markers' if a.source_photo else 'Generated solid-color encoded image fixtures')+'; native window presented pixel sampled by ImageGrab; polling/capture overhead included; hardware SD removal, GPU memory and true frame time not measured',gpu_memory_bytes=None,dataset_license=('NASA media usage guidelines; see source provenance' if a.source_photo else 'Generated by this script; project MIT license'),source_photo_sha256=hashlib.sha256(a.source_photo.read_bytes()).hexdigest() if a.source_photo else None,manifest=manifest,runs=runs,summary_ms=summary)
(a.output/'results.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
print(json.dumps(summary),flush=True)
