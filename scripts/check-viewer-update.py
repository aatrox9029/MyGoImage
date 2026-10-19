"""Exercise the native viewer with an isolated large synthetic JPEG."""
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
from PIL import Image, ImageDraw, ImageGrab

root = Path(__file__).resolve().parents[1]
out = root / 'test-output' / 'viewer-update'
out.mkdir(parents=True, exist_ok=True)
(out / 'photos').mkdir(exist_ok=True)
photo = out / 'photos' / 'large asymmetric photo.jpg'
im = Image.new('RGB', (6001, 4001), (35, 65, 100))
d = ImageDraw.Draw(im)
d.rectangle((0, 0, 2200, 4001), fill=(210, 65, 45))
d.ellipse((3800, 700, 5600, 2500), fill=(235, 195, 50))
im.save(photo, quality=90)
source_hash = hashlib.sha256(photo.read_bytes()).hexdigest()
env = os.environ.copy()
env['LOCALAPPDATA'] = str(out / 'appdata')
exe = root / 'target' / 'release' / 'mygoimage.exe'
u = ctypes.WinDLL('user32', use_last_error=True)
u.SetProcessDPIAware()
u.SetThreadDpiAwarenessContext.argtypes = [wt.HANDLE]
u.SetThreadDpiAwarenessContext.restype = wt.HANDLE
u.SetThreadDpiAwarenessContext(wt.HANDLE(-4))
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
    p = subprocess.Popen([str(exe), str(photo)], env=env, stdout=log, stderr=log)
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
            assert p.poll() is None, 'Viewer exited unexpectedly'
            u.EnumWindows(find, 0)
            time.sleep(.1)
        assert found, 'Viewer window did not appear'
        hwnd = found[0]
        u.SetWindowPos(hwnd, wt.HWND(-1), 80, 60, 1280, 800, 0x40)
        time.sleep(4)

        def capture(name):
            rect = wt.RECT()
            point = wt.POINT()
            u.GetClientRect(hwnd, ctypes.byref(rect))
            u.ClientToScreen(hwnd, ctypes.byref(point))
            shot = ImageGrab.grab((point.x, point.y, point.x + rect.right, point.y + rect.bottom))
            shot.save(out / name)
            print(name, 'client', rect.right, rect.bottom, 'origin', point.x, point.y, 'shot', shot.size)
            return shot

        def click(x, y):
            point = (y << 16) | x
            for msg, buttons in [(0x200, 0), (0x201, 1), (0x202, 0)]:
                u.PostMessageW(hwnd, msg, buttons, point)
                time.sleep(.08)
            time.sleep(.4)

        capture('01-fit.png')
        # Positions correspond to the 1280px native window; inspect screenshots.
        click(645, 92)
        capture('02-mirror.png')
        click(645, 92)
        point = wt.POINT(800, 400)
        u.PostMessageW(hwnd, 0x200, 0, (400 << 16) | 800)
        time.sleep(.15)
        u.ClientToScreen(hwnd, ctypes.byref(point))
        u.PostMessageW(hwnd, 0x20A, 120 << 16, (point.y << 16) | point.x)
        time.sleep(3)
        capture('03-wheel-zoom.png')
        u.PostMessageW(hwnd, 0x100, ord('1'), 0)
        u.PostMessageW(hwnd, 0x101, ord('1'), 0)
        time.sleep(2)
        capture('04-actual.png')
        click(980, 33)
        time.sleep(2)
        shell_check = """
        $shell = New-Object -ComObject Shell.Application
        foreach ($window in $shell.Windows()) {
            try {
                foreach ($item in $window.Document.SelectedItems()) {
                    if ($item.Name -eq 'large asymmetric photo.jpg') {
                        Write-Output $item.Path
                        $window.Quit()
                    }
                }
            } catch {}
        }
        """
        selected = subprocess.check_output(['powershell', '-NoProfile', '-Command', shell_check], text=True)
        print('Explorer selection:', selected.strip())
        assert 'large asymmetric photo.jpg' in selected, 'Explorer did not select the current image'
        u.SetWindowPos(hwnd, wt.HWND(-1), 80, 60, 900, 600, 0x40)
        time.sleep(.5)
        capture('05-minimum-window.png')
        assert hashlib.sha256(photo.read_bytes()).hexdigest() == source_hash
        settings = json.loads((out / 'appdata' / 'MyGoImage' / 'settings.json').read_text())
        assert settings['wheel_zoom'] is True
        print(f'Screenshots: {out}; source unchanged; default wheel_zoom=true')
    finally:
        if p.poll() is None:
            p.terminate()
            p.wait(timeout=10)
