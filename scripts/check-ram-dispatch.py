"""Verify a RAM main image is applied while a different large source is decoding."""
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
from PIL import Image

root=Path(__file__).resolve().parents[1]
out=root/'test-output'/('ram-dispatch-'+str(time.time_ns()))
photos=out/'photos';photos.mkdir(parents=True)
Image.new('RGB',(200,150),(30,100,160)).save(photos/'0-small.png')
large=Image.new('RGB',(8000,8000),(110,50,90))
large.save(photos/'1-large.tiff');large.close()
data=out/'appdata'/'MyGoImage';data.mkdir(parents=True)
(data/'settings.json').write_text(json.dumps(dict(image_size_limit=3)),encoding='utf-8')
env=os.environ.copy();env['LOCALAPPDATA']=str(data.parent.resolve())
u=ctypes.WinDLL('user32')
u.GetWindowThreadProcessId.argtypes=[wt.HWND,ctypes.POINTER(wt.DWORD)]
u.GetWindowTextW.argtypes=[wt.HWND,wt.LPWSTR,ctypes.c_int]
u.IsWindowVisible.argtypes=[wt.HWND]
u.PostMessageW.argtypes=[wt.HWND,wt.UINT,wt.WPARAM,wt.LPARAM]
CB=ctypes.WINFUNCTYPE(wt.BOOL,wt.HWND,wt.LPARAM);u.EnumWindows.argtypes=[CB,wt.LPARAM]
def records():
    values=[]
    for path in [data/'diagnostics.previous.jsonl',data/'diagnostics.jsonl']:
        if path.exists():
            for line in path.read_text(encoding='utf-8').splitlines():
                try:values.append(json.loads(line))
                except json.JSONDecodeError:pass
    return values
exe=root/'target/release/mygoimage.exe'
with (out/'stderr.log').open('w') as log:
    process=subprocess.Popen([str(exe),str(photos.resolve())],env=env,stdout=log,stderr=log)
    hwnd=None
    try:
        deadline=time.monotonic()+30
        active=None;initial=None
        while time.monotonic()<deadline:
            assert process.poll() is None,'viewer exited'
            windows=[]
            @CB
            def find(window,_):
                pid=wt.DWORD();u.GetWindowThreadProcessId(window,ctypes.byref(pid))
                title=ctypes.create_unicode_buffer(256);u.GetWindowTextW(window,title,len(title))
                if pid.value==process.pid and u.IsWindowVisible(window) and title.value=='MyGoImage':windows.append(window)
                return True
            u.EnumWindows(find,0)
            if windows:hwnd=windows[0]
            rows=records()
            applied=[r for r in rows if r['event']=='ui-main-apply']
            completed={r['trace']['request'] for r in rows if r['event']=='decode-complete'}
            if applied and hwnd:
                initial=applied[0]
                pending=[r for r in rows if r['event']=='decode-start' and r['trace']['request'] not in completed and r['trace']['key']!=initial['trace']['key']]
                if pending:
                    active=pending[-1]
                    break
            time.sleep(.002)
        assert active and initial,'no active source decode overlapped initial main image'
        u.PostMessageW(hwnd,0x100,0x25,0);u.PostMessageW(hwnd,0x101,0x25,0)
        deadline=time.monotonic()+10;cached=None;finished=None
        while time.monotonic()<deadline:
            rows=records()
            applied=[r for r in rows if r['event']=='ui-main-apply' and r['trace']['generation']>initial['trace']['generation']]
            completed=[r for r in rows if r['event']=='decode-complete' and r['trace']['request']==active['trace']['request']]
            if applied:cached=applied[0]
            if completed:finished=completed[0]
            if cached and finished:break
            time.sleep(.002)
        assert cached and finished,'missing main delivery or source completion'
        assert active['monotonic_us']<cached['monotonic_us']<finished['monotonic_us'],'RAM delivery waited for source decode'
        assert cached['trace']['key']==initial['trace']['key']
        assert not any(r['event']=='decode-start' and r['trace']['request']==cached['trace']['request'] for r in rows),'cached main reopened a source'
        report=dict(exe_sha256=hashlib.sha256(exe.read_bytes()).hexdigest(),scope='Synthetic 64MP TIFF occupies the serial source worker while a 200x150 PNG is reselected from RAM; Slint application timing, not GPU presentation',
                    active_source=active,source_completed=finished,ram_applied=cached,
                    ram_request_to_ui_ms=cached['duration_us']/1000,
                    delivered_before_source_completed_ms=(finished['monotonic_us']-cached['monotonic_us'])/1000,
                    no_source_job_for_ram_request=True)
        (out/'results.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
        print(json.dumps(report),flush=True);print(out,flush=True)
    finally:
        if process.poll() is None and hwnd:u.PostMessageW(hwnd,0x10,0,0)
        try:process.wait(5)
        except subprocess.TimeoutExpired:process.kill();process.wait()
