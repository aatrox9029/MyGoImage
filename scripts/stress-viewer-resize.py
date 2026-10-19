"""Native scroll/resize soak with RSS history and final visible-pixel checks.

Defaults to three ten-minute rounds. Short runs must not be reported as that
acceptance. Diagnostic files rotate: tail counters are not full-run maxima.
"""
import argparse
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import time

import psutil
from PIL import Image, ImageGrab

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--exe', type=Path, default=ROOT/'target/release/mygoimage.exe')
parser.add_argument('--photos', type=Path, default=ROOT/'test-output/photo-before/photos')
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--rounds', type=int, default=3)
parser.add_argument('--duration', type=float, default=600)
parser.add_argument('--count', type=int, choices=[6, 12, 24], default=24)
args = parser.parse_args()
assert args.rounds > 0 and args.duration >= 10
args.output.mkdir(parents=True, exist_ok=False)
files = sorted(args.photos.iterdir())
assert len(files) == 48
colors = []
for path in files:
    with Image.open(path) as photo:
        assert photo.size == (1800, 1200)
        colors.append(photo.convert('RGB').getpixel((1200, 600)))
user = ctypes.WinDLL('user32', use_last_error=True)
user.SetProcessDPIAware()
user.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
user.IsWindowVisible.argtypes = [wt.HWND]
user.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
user.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
user.ClientToScreen.argtypes = [wt.HWND, ctypes.POINTER(wt.POINT)]
user.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
user.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                             ctypes.c_int, ctypes.c_int, wt.UINT]
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


def key(hwnd, value):
    user.PostMessageW(hwnd, 0x100, value, 0)
    user.PostMessageW(hwnd, 0x101, value, 0)


def resize(hwnd, width, height):
    width = min(width, user.GetSystemMetrics(0)-60)
    height = min(height, user.GetSystemMetrics(1)-80)
    assert width >= 900 and height >= 600
    assert user.SetWindowPos(hwnd, wt.HWND(-1), 30, 30, width, height, 0x40)
    return width, height


runs = []
for round_index in range(args.rounds):
    base = args.output/f'round-{round_index+1}'
    data = base/'MyGoImage'
    data.mkdir(parents=True)
    (data/'settings.json').write_text(json.dumps(dict(
        multi_view=True, images_per_view=args.count, preload_count=24)), encoding='utf-8')
    samples, sizes = [], []
    moves = 0
    hwnd = None
    started_utc_ms = time.time_ns()//1_000_000
    started = time.perf_counter()
    with (base/'stderr.log').open('w') as log:
        process = subprocess.Popen([str(args.exe.resolve()), str(args.photos.resolve())],
                                   env=dict(os.environ, LOCALAPPDATA=str(base.resolve())),
                                   stdout=log, stderr=log)
        monitor = psutil.Process(process.pid)
        next_key, next_size, next_sample, next_progress = 1., 1., 0., 30.
        try:
            while (elapsed := time.perf_counter()-started) < args.duration:
                assert process.poll() is None, ('application exited', process.returncode)
                if hwnd is None:
                    hwnd = window(process.pid)
                if elapsed >= next_sample:
                    samples.append(dict(seconds=elapsed, rss_bytes=monitor.memory_info().rss))
                    next_sample = elapsed + .25
                if hwnd:
                    assert user.GetDpiForWindow(hwnd) == 96, 'Pixel checks require 100% DPI'
                    if elapsed >= next_key:
                        key(hwnd, 0x21 if moves % 2 else 0x22)
                        moves += 1
                        next_key = elapsed + .15
                    if elapsed >= next_size:
                        choice = len(sizes) % 4
                        width, height = [(1280, 800), (2480, 1320), (1000, 650), (1920, 1080)][choice]
                        actual = resize(hwnd, width, height)
                        sizes.append(dict(seconds=elapsed, width=actual[0], height=actual[1]))
                        next_size = elapsed + 2.
                if elapsed >= next_progress:
                    print(json.dumps(dict(round=round_index+1, seconds=round(elapsed, 1),
                                          peak_rss_bytes=max(s['rss_bytes'] for s in samples))), flush=True)
                    next_progress = elapsed + 30.
                time.sleep(.01)
            assert hwnd is not None
            resize(hwnd, 1280, 800)
            for _ in range(8):
                key(hwnd, 0x21)
            time.sleep(.25)
            rect = wt.RECT()
            user.GetClientRect(hwnd, ctypes.byref(rect))
            origin = wt.POINT(0, 0)
            user.ClientToScreen(hwnd, ctypes.byref(origin))
            columns = {6: 3, 12: 4, 24: 6}[args.count]
            cell_w, cell_h = rect.right/columns, (rect.bottom-150)/(args.count//columns)
            image_w = min(cell_w-16, (cell_h-38)*1.5)
            points = [(round((i % columns+.5)*cell_w+image_w/6),
                       round(110+(i//columns)*cell_h+8+(cell_h-38)/2)) for i in range(args.count)]
            deadline = time.perf_counter()+15
            while True:
                screenshot = ImageGrab.grab(bbox=(origin.x, origin.y, origin.x+rect.right, origin.y+rect.bottom))
                matched = sum(max(abs(a-b) for a, b in zip(screenshot.getpixel(point)[:3], colors[i])) <= 6
                              for i, point in enumerate(points))
                if matched == args.count:
                    break
                assert process.poll() is None and time.perf_counter() < deadline, ('missing final pixels', matched)
                time.sleep(.02)
            screenshot.save(base/'final.png')
        finally:
            if hwnd:
                user.PostMessageW(hwnd, 0x10, 0, 0)
            try:
                process.wait(5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise
    assert process.returncode == 0
    tail = []
    for name in ['diagnostics.previous.jsonl', 'diagnostics.jsonl']:
        path = data/name
        if path.exists():
            tail.extend(json.loads(line) for line in path.read_text(encoding='utf-8').splitlines() if line)
    tail = [r for r in tail if r['utc_ms'] >= started_utc_ms]
    counters = {event: max((r['value'] for r in tail if r['event'] == event), default=0)
                for event in ['ui-offscreen-pixel-bytes', 'ui-reuse-buffer-bytes', 'decoded-unique-bytes',
                              'ram-cache-bytes', 'ui-pixel-buffer-bytes']}
    assert counters['ui-offscreen-pixel-bytes'] <= 8*1024*1024
    assert counters['ui-reuse-buffer-bytes'] <= 8*1024*1024
    first = [s['rss_bytes'] for s in samples if 10 <= s['seconds'] < min(30, args.duration/2)]
    last = [s['rss_bytes'] for s in samples if s['seconds'] >= max(args.duration-20, args.duration/2)]
    middle = [s['rss_bytes'] for s in samples if args.duration*.5 <= s['seconds'] < args.duration*.75]
    late = [s['rss_bytes'] for s in samples if s['seconds'] >= args.duration*.8]
    result = dict(round=round_index+1, duration_seconds=args.duration, navigation_commands=moves,
                  resizes=sizes, rss_samples=samples, peak_rss_bytes=max(s['rss_bytes'] for s in samples),
                  first_band_median_rss=statistics.median(first) if first else None,
                  last_band_median_rss=statistics.median(last), final_pixels_matched=matched,
                  middle_band_median_rss=statistics.median(middle),
                  late_band_median_rss=statistics.median(late),
                  diagnostic_tail_max=counters,
                  dropped_diagnostics=max((r['dropped_total'] for r in tail), default=0), exit_code=process.returncode)
    runs.append(result)
    report = dict(exe_sha256=hashlib.sha256(args.exe.read_bytes()).hexdigest(), count=args.count,
                  scope='48 derivatives, 1800x1200, physical resize and paging, 100% DPI; full-run RSS samples; diagnostic files rotate so tail counters are not full-run maxima; final color probes are not a complete image-quality assessment',
                  manifest=[dict(name=p.name, sha256=hashlib.sha256(p.read_bytes()).hexdigest()) for p in files],
                  runs=runs)
    (args.output/'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps({k: v for k, v in result.items() if k not in ['rss_samples', 'resizes']}), flush=True)
