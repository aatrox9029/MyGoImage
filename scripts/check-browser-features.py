"""Native UI regression check using isolated synthetic images and a private portable copy."""
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
from PIL import Image, ImageDraw, ImageGrab

root = Path(__file__).resolve().parents[1]
out = root / 'test-output' / ('browser-features-' + str(time.time_ns()))
photos = out / 'photos'
app = out / 'app'
photos.mkdir(parents=True)
app.mkdir()
shutil.copy2(Path(os.environ.get('MYGO_FEATURE_EXE', root / 'target' / 'release' / 'mygoimage.exe')), app / 'mygoimage.exe')
for i in range(100):
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
if '--double-click-only' in sys.argv:
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(json.dumps({'multi_view': True, 'images_per_view': 8}), encoding='utf-8')
u = ctypes.WinDLL('user32', use_last_error=True)
u.SetProcessDPIAware()
u.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
u.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
u.IsWindowVisible.argtypes = [wt.HWND]
u.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
u.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.ClientToScreen.argtypes = [wt.HWND, ctypes.POINTER(wt.POINT)]
u.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
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
            screen = wt.POINT(x, y)
            u.ClientToScreen(hwnd, ctypes.byref(screen))
            u.SetCursorPos(screen.x, screen.y)
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
            screen = wt.POINT(x, y)
            u.ClientToScreen(hwnd, ctypes.byref(screen))
            u.SetCursorPos(screen.x, screen.y)
            u.PostMessageW(hwnd, 0x200, 0, (y << 16) | x)
            time.sleep(.2)
            point = wt.POINT(x, y)
            u.ClientToScreen(hwnd, ctypes.byref(point))
            u.PostMessageW(hwnd, 0x20A, (delta & 0xffff) << 16, (point.y << 16) | point.x)
            time.sleep(1)

        def double_click_grid():
            screen = wt.POINT(150, 230)
            u.ClientToScreen(hwnd, ctypes.byref(screen))
            u.SetCursorPos(screen.x, screen.y)
            point = (230 << 16) | 150
            for msg, buttons in [(0x200, 0), (0x201, 1), (0x202, 0), (0x201, 1), (0x202, 0)]:
                u.PostMessageW(hwnd, msg, buttons, point)
                time.sleep(.06)
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline and json.loads(settings.read_text())['multi_view']:
                time.sleep(.05)
            capture('09-double-click-result.png')
            assert not json.loads(settings.read_text())['multi_view']

        if '--double-click-only' in sys.argv:
            capture('00-before-double-click.png')
            double_click_grid()
            print(out, flush=True)
            raise SystemExit(0)

        capture('01-single.png')
        # Cursor sits on the red/blue boundary; zoom must leave that boundary anchored.
        def boundary(shot):
            return max(x for x in range(250, shot.width - 10)
                       if (lambda c: c[0] > 170 and c[1] < 100 and c[2] < 100)(shot.getpixel((x, 450))))
        before = capture('01a-before-zoom.png')
        anchor = boundary(before)
        wheel(anchor, 450, 120)
        zoomed = capture('01b-cursor-zoom.png')
        assert abs(boundary(zoomed) - anchor) <= 2, 'Zoom did not anchor to the cursor'
        key(ord('F'))
        # A held Delete must move only one photo, while companions remain by default.
        u.PostMessageW(hwnd, 0x100, 0x2e, 0)
        time.sleep(1)
        u.PostMessageW(hwnd, 0x100, 0x2e, 0x40000000)
        time.sleep(.5)
        u.PostMessageW(hwnd, 0x101, 0x2e, 0)
        assert not (photos / '000.png').exists()
        assert (photos / '000.CR3').exists()
        assert (photos / '001.png').exists(), 'Held Delete moved another photo'
        click(811, 33)
        capture('02-settings.png')
        click(32, 146)
        click(32, 270)
        click(722, 312)
        click(722, 312)
        config = json.loads(settings.read_text())
        assert config['trash_companions'] and config['multi_view']
        assert config['images_per_view'] == 8
        click(1200, 55)
        time.sleep(2)
        capture('03-grid-eight.png')
        # Quick setting shares the same persisted value as the settings page.
        for _ in range(5):
            click(158, 89)
        assert json.loads(settings.read_text())['images_per_view'] == 3
        key(0x24)
        wheel(800, 450, -120)
        capture('03a-three-per-page-partial-scroll.png')
        key(0x28)
        time.sleep(2)
        page_b = capture('03b-down-aligns-page-b.png')
        assert page_b.getpixel((250, 350))[:3] == (38, 60, 100), 'Partial scroll skipped page B (004-006)'
        key(0x28)
        time.sleep(1)
        assert capture('03c-down-page-c.png').getpixel((250, 350))[:3] == (44, 60, 100)
        key(0x26)
        assert capture('03d-up-page-b.png').getpixel((250, 350))[:3] == (38, 60, 100)
        key(0x24)
        for _ in range(5):
            click(126, 89)
        assert json.loads(settings.read_text())['images_per_view'] == 8
        wheel(900, 450, -960)
        capture('04-grid-scrolled.png')
        key(0x23)
        time.sleep(4)
        capture('05-grid-end.png')
        # End must populate the final visible photo, including results beyond the first model page.
        assert capture('05a-grid-end-loaded.png').getpixel((650, 500))[0] > 170
        key(0x24)
        key(0x2e)
        assert not (photos / '001.png').exists()
        assert not (photos / '001.CR3').exists(), 'RAW companion was not moved'
        capture('06-grid-after-delete.png')
        # Trash contains only flat, byte-identical source files, without logs or directories.
        records = []
        for copied in (app / 'trash').iterdir():
            assert copied.is_file()
            assert copied.name in source_hashes
            assert hashlib.sha256(copied.read_bytes()).hexdigest() == source_hashes[copied.name]
            records.append(copied.name)
        assert sorted(records) == ['000.png', '001.CR3', '001.png']
        assert len(list(photos.glob('*.png'))) == 98
        (out / 'results.json').write_text(json.dumps({'cursor_anchor_x': anchor,
            'zoomed_anchor_x': boundary(zoomed), 'moved': records, 'settings': config}, indent=2))
        click(811, 33)
        u.SetWindowPos(hwnd, wt.HWND(-1), 50, 50, 900, 600, 0x40)
        time.sleep(.5)
        capture('07-settings-minimum-window.png')
        p.terminate()
        p.wait(timeout=10)
        p = subprocess.Popen([str(app / 'mygoimage.exe'), str(photos)], env=env, stdout=log, stderr=log)
        found.clear()
        deadline = time.monotonic() + 20
        while not found and time.monotonic() < deadline:
            u.EnumWindows(find, 0)
            time.sleep(.1)
        assert found
        hwnd = found[0]
        u.SetWindowPos(hwnd, wt.HWND(-1), 50, 50, 1280, 800, 0x40)
        time.sleep(3)
        capture('08-restarted-grid.png')
        assert json.loads(settings.read_text()) == config
        click(90, 89)
        for character in [8, ord('1'), ord('2')]:
            key(character)
        capture('08a-typed-twelve-per-page.png')
        assert json.loads(settings.read_text())['images_per_view'] == 12
        for character in [8, 8, ord('8')]:
            key(character)
        assert json.loads(settings.read_text())['images_per_view'] == 8
        # Double-click a grid tile to return to single-image viewing.
        double_click_grid()
        capture('09-double-click-single.png')
        print(out, flush=True)
    finally:
        p.terminate()
        p.wait(timeout=10)
