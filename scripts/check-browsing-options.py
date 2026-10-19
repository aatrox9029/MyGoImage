"""Drive a private native window for browsing settings/copy smoke verification.

JSON commands on stdin: click{x,y}, key{key}, wheel{delta}, resize{w,h},
capture{name}, reopen, state, close. Only this test process/window is controlled.
"""
import argparse
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time
from PIL import Image, ImageDraw, ImageGrab

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--reuse', action='store_true', help='Reopen the existing private fixture/settings state')
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=args.reuse)
photos = args.output / 'photos'
photos.mkdir(exist_ok=args.reuse)
if not args.reuse:
    for index in range(12):
        image = Image.new('RGB', (900, 600), (30 + index * 15, 70, 120))
        ImageDraw.Draw(image).text((50, 50), f'Photo {index:02}', fill='white', font_size=60)
        image.save(photos / f'{index:03}.png')
    for stem in ['000', '001']:
        (photos / f'{stem}.raw').write_bytes(b'raw companion ' + stem.encode())
        (photos / f'{stem}.xmp').write_bytes(b'edit metadata ' + stem.encode())
data = args.output / 'appdata' / 'MyGoImage'
data.mkdir(parents=True, exist_ok=args.reuse)
destination = (args.output / 'extracted').resolve()
if not args.reuse:
    (data / 'settings.json').write_text(json.dumps({
        'preload_enabled': True, 'preload_count': 3, 'resume_last_image': True,
        'copy_folder': str(destination),
    }), encoding='utf-8')
manifest = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in photos.iterdir()}
env = os.environ.copy()
env['LOCALAPPDATA'] = str((args.output / 'appdata').resolve())
user32 = ctypes.WinDLL('user32', use_last_error=True)
user32.SetProcessDPIAware()
user32.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
user32.IsWindowVisible.argtypes = [wt.HWND]
user32.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
user32.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
user32.ClientToScreen.argtypes = [wt.HWND, ctypes.POINTER(wt.POINT)]
user32.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wt.UINT]
callback = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
user32.EnumWindows.argtypes = [callback, wt.LPARAM]
process = None
window = None
launches = []


def rows():
    text = (data / 'diagnostics.jsonl').read_text(encoding='utf-8')
    return [json.loads(line) for line in text.splitlines() if line]


def capture(name):
    rectangle = wt.RECT()
    point = wt.POINT()
    user32.GetClientRect(window, ctypes.byref(rectangle))
    user32.ClientToScreen(window, ctypes.byref(point))
    path = args.output / (name + '.png')
    ImageGrab.grab((point.x, point.y, point.x + rectangle.right, point.y + rectangle.bottom)).save(path)
    return {'image': str(path.resolve()), 'size': [rectangle.right, rectangle.bottom]}


def launch():
    global process, window
    previous = len(rows()) if (data / 'diagnostics.jsonl').exists() else 0
    log = (args.output / f'launch-{time.time_ns()}.log').open('wb')
    process = subprocess.Popen([str(ROOT / 'target/release/mygoimage.exe'), str(photos.resolve())], env=env, stdout=log, stderr=log)
    log.close()
    found = []

    @callback
    def find(handle, _):
        pid = wt.DWORD()
        user32.GetWindowThreadProcessId(handle, ctypes.byref(pid))
        if pid.value == process.pid and user32.IsWindowVisible(handle):
            title = ctypes.create_unicode_buffer(256)
            user32.GetWindowTextW(handle, title, len(title))
            if title.value == 'MyGoImage':
                found.append(handle)
        return True

    deadline = time.monotonic() + 30
    while not found and time.monotonic() < deadline:
        assert process.poll() is None, 'Native application exited during startup'
        user32.EnumWindows(find, 0)
        time.sleep(.05)
    assert found, 'Window not found'
    window = found[0]
    user32.SetWindowPos(window, wt.HWND(-1), 60, 40, 1280, 800, 0x40)
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        recent = rows()[previous:]
        apply = next((row for row in recent if row['event'] == 'ui-main-apply' or (row['event'] == 'ui-event-pump' and row['value'] > 0)), None)
        if apply:
            early = [row for row in recent if row['event'] == 'decode-start' and row['monotonic_us'] < apply['monotonic_us']]
            assert len(early) <= 1, f'{len(early)} decode jobs started before first UI image'
            launches.append({'pid': process.pid, 'decode_starts_before_first_apply': len(early),
                             'first_apply_us': apply['monotonic_us'], 'first_pixel_event': apply['event']})
            break
        time.sleep(.05)
    else:
        raise AssertionError('No first image UI upload')
    time.sleep(.5)
    return capture(f'launch-{process.pid}')


def close():
    if process and process.poll() is None:
        user32.PostMessageW(window, 0x10, 0, 0)
        process.wait(timeout=10)
        assert process.returncode == 0


def state():
    with sqlite3.connect(data / 'index.sqlite3') as db:
        selected = db.execute('SELECT relative_path FROM folder_selection').fetchall()
    current = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in photos.iterdir()}
    assert current == manifest, 'Source files changed'
    return {'settings': json.loads((data / 'settings.json').read_text(encoding='utf-8')),
            'remembered': selected,
            'copied': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in destination.iterdir()} if destination.exists() else {},
            'launches': launches, 'source_unchanged': True}


try:
    print(json.dumps(launch()), flush=True)
    for line in __import__('sys').stdin:
        command = json.loads(line)
        op = command['op']
        if op == 'click':
            point = (command['y'] << 16) | command['x']
            for message, button in [(0x200, 0), (0x201, 1), (0x202, 0)]:
                user32.PostMessageW(window, message, button, point)
                time.sleep(.05)
        elif op == 'key':
            user32.PostMessageW(window, 0x100, command['key'], 0)
            user32.PostMessageW(window, 0x101, command['key'], 0)
        elif op == 'wheel':
            x, y = command.get('x', 700), command.get('y', 450)
            user32.PostMessageW(window, 0x200, 0, (y << 16) | x)
            time.sleep(.05)
            point = wt.POINT(x, y)
            user32.ClientToScreen(window, ctypes.byref(point))
            user32.PostMessageW(window, 0x20A, (command['delta'] & 0xffff) << 16, (point.y << 16) | point.x)
        elif op == 'resize':
            user32.SetWindowPos(window, wt.HWND(-1), 60, 40, command['w'], command['h'], 0x40)
        elif op == 'reopen':
            close()
            print(json.dumps(launch()), flush=True)
        elif op == 'close':
            close()
            final = state()
            (args.output / ('results-reuse.json' if args.reuse else 'results.json')).write_text(json.dumps(final, indent=2), encoding='utf-8')
            print(json.dumps(final), flush=True)
            break
        time.sleep(.5)
        if op == 'capture':
            print(json.dumps(capture(command['name'])), flush=True)
        elif op == 'state':
            print(json.dumps(state()), flush=True)
        else:
            print(json.dumps({'done': op}), flush=True)
finally:
    close()
