"""Paired 24-image native viewport checks using the existing colored photo fixtures."""
import argparse
import ctypes
import ctypes.wintypes as wt
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import threading
import time

import psutil
from PIL import Image, ImageGrab

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--before', type=Path, required=True)
parser.add_argument('--after', type=Path, default=ROOT/'target/release/mygoimage.exe')
parser.add_argument('--photos', type=Path, default=ROOT/'test-output/photo-before/photos')
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--pairs', type=int, default=3)
parser.add_argument('--count', type=int, choices=[6, 12, 24], default=24)
parser.add_argument('--width', type=int, default=1280)
parser.add_argument('--height', type=int, default=800)
parser.add_argument('--preload-count', type=int, default=2)
parser.add_argument('--settle', type=float, default=.25)
args = parser.parse_args()
assert args.pairs > 0 and 1 <= args.preload_count <= 24 and args.settle >= 0
assert args.width >= 900 and args.height >= 600
columns = {6: 3, 12: 4, 24: 6}[args.count]
args.output.mkdir(parents=True, exist_ok=False)
files = sorted(args.photos.iterdir())
assert len(files) == 48, 'Requires the 48 numbered, 1800x1200 photo derivatives'
manifest = []
colors = []
for path in files:
    with Image.open(path) as photo:
        assert photo.size == (1800, 1200)
        colors.append(photo.convert('RGB').getpixel((1200, 600)))
    manifest.append(dict(name=path.name, sha256=hashlib.sha256(path.read_bytes()).hexdigest()))

user = ctypes.WinDLL('user32', use_last_error=True)
user.SetProcessDPIAware()
user.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
user.IsWindowVisible.argtypes = [wt.HWND]
user.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
user.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
user.ClientToScreen.argtypes = [wt.HWND, ctypes.POINTER(wt.POINT)]
user.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
user.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wt.UINT]
user.GetDpiForWindow.argtypes = [wt.HWND]
CALLBACK = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
user.EnumWindows.argtypes = [CALLBACK, wt.LPARAM]


def window(pid):
    found = []

    @CALLBACK
    def visit(hwnd, _):
        owner = wt.DWORD()
        user.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        title = ctypes.create_unicode_buffer(256)
        user.GetWindowTextW(hwnd, title, len(title))
        if owner.value == pid and user.IsWindowVisible(hwnd) and title.value == 'MyGoImage':
            found.append(hwnd)
        return True

    user.EnumWindows(visit, 0)
    return found[0] if found else None


def percentiles(values):
    ordered = sorted(values)
    return dict(n=len(ordered), **{f'p{int(q*100)}': ordered[math.ceil((len(ordered)-1)*q)]
                                 for q in [.5, .95]})


records = []
for pair in range(args.pairs):
    order = ['before', 'after'] if pair % 2 == 0 else ['after', 'before']
    for variant in order:
        base = args.output/f'{pair}-{variant}'
        data = base/'MyGoImage'
        data.mkdir(parents=True)
        (data/'settings.json').write_text(json.dumps(dict(multi_view=True, images_per_view=args.count, preload_count=args.preload_count)), encoding='utf-8')
        for cache in ['cold', 'warm']:
            env = dict(os.environ, LOCALAPPDATA=str(base.resolve()))
            stop = threading.Event()
            peak = [0]
            samples = []
            hwnd = None
            with (base/f'{cache}.stderr.log').open('w') as log:
                started_utc_ms = time.time_ns() // 1_000_000
                started = time.perf_counter()
                process = subprocess.Popen([str(getattr(args, variant).resolve()), str(args.photos.resolve())],
                                           env=env, stdout=log, stderr=log)
                monitor = psutil.Process(process.pid)

                def watch():
                    while not stop.wait(.01):
                        try:
                            peak[0] = max(peak[0], monitor.memory_info().rss)
                        except psutil.Error:
                            return

                watcher = threading.Thread(target=watch)
                watcher.start()
                try:
                    deadline = started + 30
                    while hwnd is None and time.perf_counter() < deadline:
                        assert process.poll() is None, 'Application exited'
                        hwnd = window(process.pid)
                        time.sleep(.01)
                    assert hwnd is not None, 'Window timeout'
                    user.SetWindowPos(hwnd, wt.HWND(-1), 30, 30, args.width, args.height, 0x40)
                    assert user.GetDpiForWindow(hwnd) == 96, 'Pixel probes require 100% display scaling'
                    client = wt.RECT()
                    user.GetClientRect(hwnd, ctypes.byref(client))
                    origin = wt.POINT(0, 0)
                    user.ClientToScreen(hwnd, ctypes.byref(origin))
                    bbox = (origin.x, origin.y, origin.x+client.right, origin.y+client.bottom)
                    # 66px toolbar, 44px grid controls, 40px status.
                    cell_w = client.right / columns
                    cell_h = (client.bottom - 150) / (args.count // columns)
                    image_w = min(cell_w-16, (cell_h-38)*1.5)
                    points = [(round((i % columns+.5)*cell_w+image_w/6),
                               round(110+(i//columns)*cell_h+8+(cell_h-38)/2)) for i in range(args.count)]
                    for step, first in enumerate([0, args.count, 0, args.count, 0, args.count]):
                        sent = started if step == 0 else time.perf_counter()
                        if step:
                            key = 0x22 if first else 0x21  # PageDown / PageUp
                            user.PostMessageW(hwnd, 0x100, key, 0)
                            user.PostMessageW(hwnd, 0x101, key, 0)
                        deadline = time.perf_counter()+15
                        while True:
                            screenshot = ImageGrab.grab(bbox=bbox)
                            matched = sum(max(abs(a-b) for a, b in zip(screenshot.getpixel(point)[:3], colors[first+i])) <= 6
                                          for i, point in enumerate(points))
                            if matched == args.count:
                                break
                            assert process.poll() is None, 'Application exited'
                            assert time.perf_counter() < deadline, f'{variant}/{cache}: only {matched}/{args.count} pixels matched page {first}'
                            time.sleep(.01)
                        samples.append(dict(scenario=f'startup-full{args.count}' if step == 0 else f'page-full{args.count}',
                                            first=first, ms=(time.perf_counter()-sent)*1000, matched=matched))
                        if step in [0, 5]:
                            screenshot.save(base/f'{cache}-{step}.png')
                        time.sleep(args.settle)
                finally:
                    if hwnd:
                        user.PostMessageW(hwnd, 0x10, 0, 0)
                    try:
                        process.wait(5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                        raise
                    stop.set()
                    watcher.join()
            assert process.returncode == 0, process.returncode
            traces = []
            for path in [data/'diagnostics.previous.jsonl', data/'diagnostics.jsonl']:
                if path.exists():
                    traces.extend(json.loads(line) for line in path.read_text(encoding='utf-8').splitlines() if line)
            traces = [r for r in traces if r['utc_ms'] >= started_utc_ms]
            (base/f'{cache}.diagnostics.json').write_text(json.dumps(traces), encoding='utf-8')
            memory = {event: max((r['value'] for r in traces if r['event'] == event), default=None)
                      for event in ['ram-cache-bytes', 'decoded-result-bytes', 'decoded-unique-bytes', 'ui-pixel-buffer-bytes', 'ui-reuse-buffer-bytes', 'ui-offscreen-pixel-bytes']}
            record = dict(pair=pair+1, variant=variant, cache=cache, peak_rss_bytes=peak[0],
                          max_accounted_bytes=memory, dropped_diagnostics=max((r['dropped_total'] for r in traces), default=0),
                          samples=samples)
            records.append(record)
            print(json.dumps(record), flush=True)

summary = {}
for variant in ['before', 'after']:
    runs = [r for r in records if r['variant'] == variant]
    samples = [s for r in runs for s in r['samples']]
    summary[variant] = {scenario: percentiles([s['ms'] for s in samples if s['scenario'] == scenario])
                        for scenario in [f'startup-full{args.count}', f'page-full{args.count}']}
    summary[variant]['rss_bytes'] = percentiles([r['peak_rss_bytes'] for r in runs])
report = dict(date=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(),
              scope='48 photo derivatives; 100% DPI; one color probe per image; screenshot polling included; alternating paired order; cold application cache then warm restart; OS cache not cleared; CPU buffer metrics overlap ownership and exclude GPU/codec allocations',
              settings=dict(count=args.count,width=args.width,height=args.height,preload_count=args.preload_count,settle_seconds=args.settle),
              manifest=manifest, before_sha256=hashlib.sha256(args.before.read_bytes()).hexdigest(),
              after_sha256=hashlib.sha256(args.after.read_bytes()).hexdigest(), records=records, summary=summary)
(args.output/'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
print(json.dumps(summary), flush=True)
