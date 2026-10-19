"""Read-only native SD-card browsing smoke; never flushes the OS file cache."""
import argparse
import ctypes
import ctypes.wintypes as wt
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import sqlite3
import subprocess
import threading
import time

import psutil
from PIL import Image, ImageGrab

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--source', type=Path, required=True)
parser.add_argument('--runs', type=int, default=3)
args = parser.parse_args()
if args.runs < 1:
    parser.error('--runs must be at least 1')
source = args.source.resolve(strict=True)
out = ROOT / 'test-output' / f'sd-card-{time.time_ns()}'
out.mkdir(parents=True)
exe = ROOT / 'target/release/mygoimage.exe'

def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def manifest():
    return {str(p.relative_to(source)): dict(bytes=p.stat().st_size, sha256=digest(p))
            for p in sorted(source.rglob('*')) if p.is_file()}

before = manifest()
jpgs = sorted(p for p in source.iterdir() if p.suffix.lower() in ('.jpg', '.jpeg'))
assert jpgs, 'No JPEG files'
dimensions = {}
for path in jpgs:
    with Image.open(path) as im:
        dimensions[path.name] = im.size

u = ctypes.WinDLL('user32')
u.SetProcessDPIAware()
u.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
u.IsWindowVisible.argtypes = [wt.HWND]
u.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
u.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
u.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wt.UINT]
CB = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
u.EnumWindows.argtypes = [CB, wt.LPARAM]

def window(pid):
    found = []
    @CB
    def callback(hwnd, _):
        owner = wt.DWORD()
        u.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        title = ctypes.create_unicode_buffer(256)
        u.GetWindowTextW(hwnd, title, len(title))
        if owner.value == pid and u.IsWindowVisible(hwnd) and title.value == 'MyGoImage':
            found.append(hwnd)
        return True
    u.EnumWindows(callback, 0)
    return found[0] if found else None

def percentile(values):
    values = sorted(values)
    return {f'p{q}': values[math.ceil((len(values)-1)*q/100)] for q in (50, 95, 99)} if values else None

report = dict(date=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(),
              source=str(source), exe_sha256=digest(exe), manifest=before,
              jpeg_dimensions=dimensions, os_cache_flushed=False,
              scope=f'Read-only real SD folder; {args.runs} independent application caches with warm restarts. '
                    'Source hashing warms OS cache. Sequential navigation at 500 ms intervals, '
                    'End/Home and 100% requests. UI diagnostics are not presented-frame or navigation latency. '
                    'No physical removal, deletion, RAW decoding or GPU-memory measurement.', runs=[])
(out/'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
print(f'Artifacts: {out}', flush=True)
for run in range(args.runs):
    base = out / f'appdata-{run}'
    for cache in ('cold-application-cache', 'warm-application-cache'):
        data = base / 'MyGoImage'
        for name in ('diagnostics.jsonl', 'diagnostics.previous.jsonl'):
            path = data/name
            if path.exists():
                path.rename(out/f'{run}-{cache}-prior-{name}')
        env = os.environ.copy()
        env['LOCALAPPDATA'] = str(base)
        stop = threading.Event()
        sample = dict(peak_rss_bytes=0, cpu_seconds=0, process_read_bytes=0)
        with (out/f'{run}-{cache}.stderr.log').open('w') as log:
            p = subprocess.Popen([str(exe), str(source)], env=env, stdout=log, stderr=log)
            proc = psutil.Process(p.pid)
            def monitor():
                while not stop.wait(.02):
                    try:
                        sample['peak_rss_bytes'] = max(sample['peak_rss_bytes'], proc.memory_info().rss)
                        cpu = proc.cpu_times()
                        sample['cpu_seconds'] = cpu.user + cpu.system
                        sample['process_read_bytes'] = proc.io_counters().read_bytes
                    except psutil.Error:
                        break
            watcher = threading.Thread(target=monitor)
            watcher.start()
            hwnd = None
            try:
                deadline = time.perf_counter()+30
                while hwnd is None and time.perf_counter()<deadline:
                    assert p.poll() is None, 'App exited before opening'
                    hwnd = window(p.pid)
                    time.sleep(.02)
                assert hwnd, 'Window timeout'
                u.SetWindowPos(hwnd, wt.HWND(-1), 30, 30, 1280, 800, 0x40)
                def key(code):
                    assert p.poll() is None, 'App exited during browsing'
                    u.PostMessageW(hwnd, 0x100, code, 0)
                    u.PostMessageW(hwnd, 0x101, code, 0)
                def capture(name):
                    ImageGrab.grab(bbox=(30,30,1310,830)).save(out/f'{run}-{cache}-{name}.png')
                time.sleep(2)
                capture('first')
                for _ in range(len(jpgs)-1):
                    key(0x27)
                    time.sleep(.5)
                key(0x23)
                time.sleep(1)
                capture('last')
                key(ord('1'))
                time.sleep(1)
                capture('100-percent')
                key(0x24)
                time.sleep(.5)
            finally:
                if hwnd and p.poll() is None:
                    u.PostMessageW(hwnd, 0x10, 0, 0)
                try:
                    p.wait(5)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait()
                    raise AssertionError('Graceful shutdown timeout')
                finally:
                    stop.set()
                    watcher.join()
            assert p.returncode == 0, f'Exit code {p.returncode}'
        records = []
        for name in ('diagnostics.previous.jsonl', 'diagnostics.jsonl'):
            path = data/name
            if path.exists():
                records.extend(json.loads(line) for line in path.read_text().splitlines() if line)
        with sqlite3.connect(data/'index.sqlite3') as db:
            indexed = sorted(row[0] for row in db.execute('SELECT relative_path FROM images'))
        assert indexed == sorted(p.name for p in jpgs), indexed
        pumps = [r for r in records if r['event']=='ui-event-pump']
        assert any(r['value']>0 for r in pumps), 'No UI image upload'
        result = dict(run=run+1, cache=cache, indexed_images=len(indexed), **sample,
                      ui_event_pump_ms=percentile([r['duration_us']/1000 for r in pumps]),
                      ui_timer_interval_ms=percentile([r['duration_us']/1000 for r in records if r['event']=='ui-timer-interval']),
                      failed_diagnostics=[r for r in records if not r['ok']],
                      dropped_diagnostics=max((r['dropped_total'] for r in records), default=0))
        report['runs'].append(result)
        (out/'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
        print(json.dumps(result), flush=True)
after = manifest()
report['source_unchanged'] = before == after
(out/'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
assert report['source_unchanged'], 'Source manifest changed'
print('PASS: all source hashes unchanged; native browsing and index checks completed.', flush=True)
