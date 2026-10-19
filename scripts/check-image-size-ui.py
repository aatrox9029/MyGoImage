"""Native UI regression check using isolated synthetic images and a private portable copy."""
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
from PIL import Image, ImageDraw, ImageGrab

root = Path(__file__).resolve().parents[1]
out = root / 'test-output' / ('image-size-settings-' + str(time.time_ns()))
photos = out / 'photos'
app = out / 'app'
photos.mkdir(parents=True)
app.mkdir()
shutil.copy2(Path(os.environ.get('MYGO_FEATURE_EXE', root / 'target' / 'release' / 'mygoimage.exe')), app / 'mygoimage.exe')
for i in range(1):
    im = Image.new('RGB', (1200, 800), (30 + i * 2, 60, 100))
    draw = ImageDraw.Draw(im)
    draw.rectangle((0, 0, 400, 800), fill=(210, 65, 45))
    draw.ellipse((800, 100, 1100, 400), fill=(235, 195, 50))
    draw.text((40, 40), f'Photo {i:03}', fill='white', font_size=40)
    im.save(photos / f'{i:03}.png')
    (photos / f'{i:03}.CR3').write_bytes(f'RAW companion {i}'.encode())
source_hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in photos.iterdir()}
env = os.environ.copy()
env['LOCALAPPDATA'] = str(out / 'appdata')
settings = out / 'appdata' / 'MyGoImage' / 'settings.json'
u = ctypes.WinDLL('user32', use_last_error=True)
u.SetProcessDPIAware()
u.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
u.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
u.IsWindowVisible.argtypes = [wt.HWND]
u.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
u.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.ClientToScreen.argtypes = [wt.HWND, ctypes.POINTER(wt.POINT)]
u.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wt.UINT]
callback_type = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
u.EnumWindows.argtypes = [callback_type, wt.LPARAM]
with (out / 'stderr.log').open('w') as log:
    p = subprocess.Popen([str(app / 'mygoimage.exe'), str(photos)], env=env, stdout=log, stderr=log)
    try:
        found = []
        @callback_type
        def find(hwnd, _):
            pid = wt.DWORD()
            u.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
            title = ctypes.create_unicode_buffer(256)
            u.GetWindowTextW(hwnd, title, len(title))
            if pid.value == p.pid and title.value == 'MyGoImage' and u.IsWindowVisible(hwnd):
                found.append(hwnd)
            return True
        deadline = time.monotonic() + 30
        while not found and time.monotonic() < deadline:
            assert p.poll() is None, 'Viewer exited'
            u.EnumWindows(find, 0)
            time.sleep(.1)
        assert found
        hwnd = found[0]
        u.SetWindowPos(hwnd, wt.HWND(-1), 50, 50, 1280, 800, 0x40)
        time.sleep(3)

        def capture(name):
            rect = wt.RECT()
            point = wt.POINT()
            u.GetClientRect(hwnd, ctypes.byref(rect))
            u.ClientToScreen(hwnd, ctypes.byref(point))
            shot = ImageGrab.grab((point.x, point.y, point.x + rect.right, point.y + rect.bottom))
            shot.save(out / name)
            return shot

        def click(x, y):
            point = (y << 16) | x
            for msg, buttons in [(0x200, 0), (0x201, 1), (0x202, 0)]:
                u.PostMessageW(hwnd, msg, buttons, point)
                time.sleep(.08)
            time.sleep(.6)

        def key(code):
            u.PostMessageW(hwnd, 0x100, code, 0)
            u.PostMessageW(hwnd, 0x101, code, 0)
            time.sleep(.7)

        def wheel(x, y, delta):
            u.PostMessageW(hwnd, 0x200, 0, (y << 16) | x)
            time.sleep(.2)
            point = wt.POINT(x, y)
            u.ClientToScreen(hwnd, ctypes.byref(point))
            u.PostMessageW(hwnd, 0x20A, (delta & 0xffff) << 16, (point.y << 16) | point.x)
            time.sleep(1)

        click(811, 33)
        capture('01-settings.png')
        click(620, 420)
        capture('02-options.png')
        point = wt.POINT(150, 585)
        u.ClientToScreen(hwnd, ctypes.byref(point))
        u.SetCursorPos(point.x, point.y)
        u.mouse_event(2, 0, 0, 0, 0)
        u.mouse_event(4, 0, 0, 0, 0)
        time.sleep(1)
        assert json.loads(settings.read_text())['image_size_limit'] == 4
        capture('03-unlimited.png')
        u.SetWindowPos(hwnd, wt.HWND(-1), 50, 50, 900, 600, 0x40)
        time.sleep(1)
        capture('04-minimum-window.png')
        p.terminate()
        p.wait(timeout=10)
        p = subprocess.Popen([str(app / 'mygoimage.exe'), str(photos)], env=env, stdout=log, stderr=log)
        found.clear()
        deadline = time.monotonic() + 30
        while not found and time.monotonic() < deadline:
            assert p.poll() is None
            u.EnumWindows(find, 0)
            time.sleep(.1)
        assert found
        hwnd = found[0]
        u.SetWindowPos(hwnd, wt.HWND(-1), 50, 50, 1280, 800, 0x40)
        time.sleep(2)
        click(811, 33)
        capture('05-restarted.png')
        assert json.loads(settings.read_text())['image_size_limit'] == 4
        print(out.resolve(), flush=True)
        time.sleep(2)
    finally:
        p.terminate()
        p.wait(timeout=10)
