"""Bounded native SD/photo session, with image-content checks at actual UI scale.

Reads originals only. Pillow reference preparation precedes timing and warms OS
file caches; this is cold *application* cache, not physical cold SD I/O. RAW files
are listed but excluded because the viewer does not support them.
"""
import argparse
import ctypes as c
import ctypes.wintypes as wt
import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import time

import psutil
from PIL import Image, ImageChops, ImageOps, ImageStat


def percentile(values, q):
    return sorted(values)[math.ceil((len(values)-1)*q)] if values else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--exe', type=Path, required=True)
    parser.add_argument('--photos', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--scale', type=float, default=1.25)
    parser.add_argument('--duration', type=float, default=120)
    parser.add_argument('--count', type=int, choices=[6, 12, 24], default=12)
    args = parser.parse_args()
    assert args.duration >= 10 and args.scale > 0
    args.output.mkdir(parents=True, exist_ok=False)
    supported = {'.jpg', '.jpeg', '.png', '.webp', '.tiff', '.tif', '.bmp', '.gif'}
    files = sorted((p for p in args.photos.iterdir() if p.suffix.lower() in supported),
                   key=lambda p: p.name.casefold())
    assert len(files) > args.count
    refs, manifest = [], []
    for path in files:
        with Image.open(path) as original:
            entry = dict(name=path.name, bytes=path.stat().st_size,
                         dimensions=original.size, orientation=original.getexif().get(274, 1),
                         icc_bytes=len(original.info.get('icc_profile', b'')),
                         sha256=hashlib.sha256(path.read_bytes()).hexdigest())
            # Current SD corpus is untagged JPEG; reject an unsupported reference
            # path instead of treating unmanaged ICC pixels as a valid oracle.
            assert not entry['icc_bytes'], 'ICC reference requires color-managed comparison'
            # Content matching uses a 32px reference, so a full 24MP decode here
            # only lengthens setup. JPEG draft keeps the original file intact.
            original.draft('RGB', (768, 768))
            reference = ImageOps.exif_transpose(original).convert('RGB')
            refs.append((reference.size, reference.resize((64, 64))))
            manifest.append(entry)
    user = c.WinDLL('user32', use_last_error=True)
    user.SetProcessDPIAware()
    user.GetWindowThreadProcessId.argtypes = [wt.HWND, c.POINTER(wt.DWORD)]
    user.IsWindowVisible.argtypes = [wt.HWND]
    user.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, c.c_int]
    user.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
    user.GetClientRect.argtypes = [wt.HWND, c.POINTER(wt.RECT)]
    user.ClientToScreen.argtypes = [wt.HWND, c.POINTER(wt.POINT)]
    user.SetWindowPos.argtypes = [wt.HWND, wt.HWND, c.c_int, c.c_int, c.c_int, c.c_int, wt.UINT]
    callback = c.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    user.EnumWindows.argtypes = [callback, wt.LPARAM]

    def find_window(pid):
        found = []

        @callback
        def visit(hwnd, _):
            owner = wt.DWORD()
            user.GetWindowThreadProcessId(hwnd, c.byref(owner))
            title = c.create_unicode_buffer(256)
            user.GetWindowTextW(hwnd, title, 256)
            if owner.value == pid and user.IsWindowVisible(hwnd) and title.value == 'MyGoImage':
                found.append(hwnd)
            return True

        user.EnumWindows(visit, 0)
        return found[0] if found else None

    def key(hwnd, value):
        user.PostMessageW(hwnd, 0x100, value, 0)
        user.PostMessageW(hwnd, 0x101, value, 0)

    # Import here to keep fixture preparation separate from the measurement.
    from PIL import ImageGrab
    data = args.output/'appdata'/'MyGoImage'
    data.mkdir(parents=True)
    (data/'settings.json').write_text(json.dumps(dict(
        multi_view=True, images_per_view=args.count, preload_count=24)), encoding='utf-8')
    log = (args.output/'stderr.log').open('w')
    start_utc_us = time.time_ns()//1000
    started = time.perf_counter()
    process = subprocess.Popen([str(args.exe.resolve()), str(args.photos.resolve())],
                               env=dict(os.environ, LOCALAPPDATA=str(data.parent.resolve()),
                                        SLINT_SCALE_FACTOR=str(args.scale)), stdout=log, stderr=log)
    popen_ms = (time.perf_counter()-started)*1000
    monitor = psutil.Process(process.pid)
    hwnd, samples, checks, resizes = None, [], [], []
    first, commands, next_sample = 0, 0, 0
    columns = {6: 3, 12: 4, 24: 6}[args.count]

    def read_traces():
        rows = []
        for name in ['diagnostics.previous.jsonl', 'diagnostics.jsonl']:
            path = data/name
            if path.exists():
                for line in path.read_text(encoding='utf-8').splitlines():
                    try:
                        rows.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass  # The live writer may not have finished its last line.
        return rows

    def resize(width, height):
        width = min(width, user.GetSystemMetrics(0)-60)
        height = min(height, user.GetSystemMetrics(1)-80)
        assert user.SetWindowPos(hwnd, wt.HWND(-1), 30, 30, width, height, 0x40)
        resizes.append(dict(seconds=time.perf_counter()-started, width=width, height=height))

    def check_page(expected_first, timeout=20):
        sent = time.perf_counter()
        while time.perf_counter()-sent < timeout:
            assert process.poll() is None, ('application exited', process.returncode)
            rect, origin = wt.RECT(), wt.POINT(0, 0)
            user.GetClientRect(hwnd, c.byref(rect))
            user.ClientToScreen(hwnd, c.byref(origin))
            shot = ImageGrab.grab(bbox=(origin.x, origin.y, origin.x+rect.right, origin.y+rect.bottom))
            scale = args.scale
            cell_w = rect.right/columns
            cell_h = (rect.bottom-150*scale)/(args.count//columns)
            errors = []
            competitors = []
            for i in range(min(args.count, len(refs)-expected_first)):
                (iw, ih), ref = refs[expected_first+i]
                ratio = min((cell_w-16*scale)/iw, (cell_h-38*scale)/ih)
                dw, dh = iw*ratio, ih*ratio
                x = (i % columns+.5)*cell_w
                y = 110*scale+(i//columns)*cell_h+8*scale+(cell_h-38*scale)/2
                box = tuple(round(v) for v in (x-dw*.4, y-dh*.4, x+dw*.4, y+dh*.4))
                actual = shot.crop(box).convert('RGB').resize((32, 32))
                # Center 80% avoids contain-edge sampling and thin borders.
                def error(candidate):
                    expected = candidate.crop((6, 6, 58, 58)).resize((32, 32))
                    return statistics.mean(ImageStat.Stat(ImageChops.difference(actual, expected)).mean)
                errors.append(error(ref))
                competitors.append(min(error(other) for j, (_, other) in enumerate(refs)
                                       if j != expected_first+i))
            if errors and max(errors) < 18 and all(a+1 < b for a, b in zip(errors, competitors)):
                return shot, errors, (time.perf_counter()-sent)*1000
            time.sleep(.03)
        shot.save(args.output/'failed.png')
        raise AssertionError(('photo content mismatch', expected_first, errors, competitors))

    try:
        while hwnd is None and time.perf_counter()-started < 30:
            assert process.poll() is None
            hwnd = find_window(process.pid)
            time.sleep(.01)
        assert hwnd
        window_found_ms = (time.perf_counter()-started)*1000
        resize(2480, 1320)
        shot, errors, _ = check_page(0)
        checks.append(dict(first=0, seconds=time.perf_counter()-started, errors=errors,
                           startup_ms=(time.perf_counter()-started)*1000))
        shot.save(args.output/'first.png')
        startup_traces = read_traces()
        (args.output/'startup.diagnostics.json').write_text(json.dumps(startup_traces), encoding='utf-8')
        # Explicit regression: resize a non-first page, where a stale absolute
        # pixel offset used to expose the previous page above the selected row.
        key(hwnd, 0x22)
        check_page(args.count)
        resize(2000, 1250)
        shot, errors, ms = check_page(args.count)
        checks.append(dict(first=args.count, seconds=time.perf_counter()-started,
                           errors=errors, wait_for_content_ms=ms, scenario='resize-second-page'))
        shot.save(args.output/'resized-second-page.png')
        first = args.count
        commands = 1
        next_page, next_resize, next_check, next_progress = 1., 2., 3., 30.
        while (elapsed := time.perf_counter()-started) < args.duration:
            assert process.poll() is None
            if elapsed >= next_sample:
                samples.append(dict(seconds=elapsed, rss_bytes=monitor.memory_info().rss))
                next_sample = elapsed+.25
            if elapsed >= next_page:
                first = args.count if first == 0 else 0
                key(hwnd, 0x22 if first else 0x21)
                commands += 1
                next_page = elapsed+.4
            if elapsed >= next_resize:
                size = [(2480, 1320), (2000, 1250), (2400, 1300)][len(resizes) % 3]
                resize(*size)
                next_resize = elapsed+3
            if elapsed >= next_check:
                shot, errors, ms = check_page(first)
                checks.append(dict(first=first, seconds=time.perf_counter()-started, errors=errors,
                                   wait_for_content_ms=ms))
                next_check = time.perf_counter()-started+5
            if elapsed >= next_progress:
                print(json.dumps(dict(seconds=round(elapsed, 1), checks=len(checks), commands=commands)), flush=True)
                next_progress = elapsed+30
            time.sleep(.01)
        key(hwnd, 0x21)
        resize(2480, 1320)
        shot, errors, ms = check_page(0)
        checks.append(dict(first=0, seconds=time.perf_counter()-started, errors=errors,
                           wait_for_content_ms=ms))
        shot.save(args.output/'final.png')
        samples.append(dict(seconds=time.perf_counter()-started, rss_bytes=monitor.memory_info().rss))
    except Exception as error:
        (args.output/'failure.json').write_text(json.dumps(dict(
            error=repr(error), seconds=time.perf_counter()-started,
            rss_samples=samples, content_checks=checks, resizes=resizes,
            navigation_commands=commands, expected_first=first,
            exe_sha256=hashlib.sha256(args.exe.read_bytes()).hexdigest()), indent=2), encoding='utf-8')
        raise
    finally:
        if hwnd:
            user.PostMessageW(hwnd, 0x10, 0, 0)
        try:
            process.wait(5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            raise
        log.close()
    assert process.returncode == 0
    traces = read_traces()
    (args.output/'tail.diagnostics.json').write_text(json.dumps(traces), encoding='utf-8')
    scale_values = sorted({r['value'] for r in startup_traces+traces if r['event'] == 'ui-scale-factor-milli'})
    if scale_values:
        assert round(args.scale*1000) in scale_values, scale_values
    counters = {event: max((r['value'] for r in traces if r['event'] == event), default=0)
                for event in ['ui-offscreen-pixel-bytes', 'ui-reuse-buffer-bytes', 'ui-pixel-buffer-bytes']}
    assert counters['ui-offscreen-pixel-bytes'] <= 8*1024*1024
    assert counters['ui-reuse-buffer-bytes'] <= 8*1024*1024
    bands = {}
    for label, low, high in [('early', .15, .35), ('middle', .5, .7), ('late', .8, 1.01)]:
        values = [r['rss_bytes'] for r in samples if args.duration*low <= r['seconds'] < args.duration*high]
        bands[label] = statistics.median(values) if values else None
    # Check that the original SD photos were unchanged throughout the session.
    assert all(hashlib.sha256(p.read_bytes()).hexdigest() == entry['sha256']
               for p, entry in zip(files, manifest)), 'Source image changed during session'
    report = dict(exe_sha256=hashlib.sha256(args.exe.read_bytes()).hexdigest(),
                  source=str(args.photos.resolve()), manifest=manifest,
                  excluded_files=[p.name for p in args.photos.iterdir() if p.suffix.lower() not in supported],
                  scope='Real originals read from source, EXIF-oriented content matching; cold app cache; OS cache warmed by reference preparation; Slint scale override, not Windows monitor DPI change; RSS excludes dedicated GPU memory; diagnostic maxima cover retained tail only',
                  scale=args.scale, observed_scale_milli=scale_values, count=args.count,
                  originals_unchanged=True,
                  duration_seconds=args.duration, navigation_commands=commands, content_checks=checks,
                  startup_timing=dict(popen_ms=popen_ms, window_found_ms=window_found_ms,
                      spawn_to_main_wall_clock_ms=next(((r['value']-start_utc_us)/1000 for r in startup_traces
                          if r['event'] == 'startup-main-entry-utc-us'), None)),
                  rss_samples=samples, median_rss_bands=bands, peak_sampled_rss_bytes=max(r['rss_bytes'] for r in samples),
                  resizes=resizes, diagnostic_tail_max=counters,
                  dropped_diagnostics=max((r['dropped_total'] for r in traces), default=0), exit_code=process.returncode)
    (args.output/'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps({k: v for k, v in report.items() if k not in ['manifest', 'rss_samples', 'content_checks', 'resizes']}), flush=True)


if __name__ == '__main__':
    main()
