"""Verify fresh staging, ZIP hashes and native launch with only the Windows system PATH."""
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import tomllib
import uuid
import zipfile
from PIL import Image, ImageGrab

root = Path(__file__).resolve().parents[1]
run = root / 'test-output' / ('optimized-portable-' + str(time.time_ns()))
run.mkdir(parents=True)
version = tomllib.loads((root / 'Cargo.toml').read_text(encoding='utf-8'))['workspace']['package']['version']
legacy = root / 'dist' / f'MyGoImage-{version}-windows-x64'
legacy.mkdir(exist_ok=True)
marker = legacy / ('stale-test-' + uuid.uuid4().hex + '.txt')
marker.write_text('Created only to verify staging isolation', encoding='utf-8')
try:
    subprocess.run(['powershell', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File',
                    str(root / 'scripts/package-windows.ps1'), '-SkipBuild'], cwd=root, check=True)
    assert marker.is_file(), 'Packaging modified the existing portable directory'
    archive = root / 'dist' / f'MyGoImage-{version}-windows-x64.zip'
    with zipfile.ZipFile(archive) as package:
        assert package.testzip() is None
        assert marker.name not in package.namelist()
        assert all(marker.name not in entry for entry in package.namelist())
        manifest = json.loads(package.read('build-manifest.json'))
        for name, expected in manifest['files'].items():
            assert hashlib.sha256(package.read(name)).hexdigest() == expected, name
        app = run / 'app'
        for name in package.namelist():
            assert (app / name).resolve().is_relative_to(app.resolve()), name
        package.extractall(app)
    executable = app / 'mygoimage.exe'
    exe_sha = hashlib.sha256(executable.read_bytes()).hexdigest()
    built_binary = root / 'target' / manifest['target'] / 'release/mygoimage.exe'
    if not built_binary.is_file():
        built_binary = root / 'target/release/mygoimage.exe'
    assert exe_sha == hashlib.sha256(built_binary.read_bytes()).hexdigest()
    other_target = 'x86_64-pc-windows-msvc' if manifest['target'].endswith('gnu') else 'x86_64-pc-windows-gnu'
    wrong_target_rejected = None
    if not (root / 'target' / other_target / 'release/mygoimage.exe').is_file():
        rejected = subprocess.run(['powershell', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File',
                                  str(root / 'scripts/package-windows.ps1'), '-SkipBuild', '-Target', other_target],
                                 cwd=root, capture_output=True, text=True)
        assert rejected.returncode != 0 and 'No release binary exists' in rejected.stderr
        wrong_target_rejected = True
    photo = run / 'sample.png'
    Image.new('RGB', (1200, 800), (25, 90, 135)).save(photo)
    env = os.environ.copy()
    env['LOCALAPPDATA'] = str(run / 'appdata')
    system_root = os.environ.get('SystemRoot', r'C:\Windows')
    env['PATH'] = os.pathsep.join([str(Path(system_root) / 'System32'), system_root])
    u = ctypes.WinDLL('user32')
    u.SetProcessDPIAware()
    u.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
    u.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
    u.IsWindowVisible.argtypes = [wt.HWND]
    u.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
    u.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wt.UINT]
    u.ClientToScreen.argtypes = [wt.HWND, ctypes.POINTER(wt.POINT)]
    u.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
    callback_type = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    u.EnumWindows.argtypes = [callback_type, wt.LPARAM]
    with (run / 'stderr.log').open('w') as log:
        process = subprocess.Popen([str(executable), str(photo)], cwd=app, env=env, stdout=log, stderr=log)
        try:
            windows = []
            @callback_type
            def find(hwnd, _):
                owner = wt.DWORD()
                u.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
                title = ctypes.create_unicode_buffer(256)
                u.GetWindowTextW(hwnd, title, len(title))
                if owner.value == process.pid and title.value == 'MyGoImage' and u.IsWindowVisible(hwnd):
                    windows.append(hwnd)
                return True
            deadline = time.monotonic() + 20
            while not windows and time.monotonic() < deadline:
                assert process.poll() is None, 'Portable process exited before showing its window'
                u.EnumWindows(find, 0)
                time.sleep(.02)
            assert windows, 'Portable window did not appear'
            hwnd = windows[0]
            u.SetWindowPos(hwnd, wt.HWND(-1), 30, 30, 1280, 800, 0x40)
            point = wt.POINT(900, 420)
            u.ClientToScreen(hwnd, ctypes.byref(point))
            presented = False
            while time.monotonic() < deadline:
                color = ImageGrab.grab(bbox=(point.x, point.y, point.x + 1, point.y + 1)).getpixel((0, 0))[:3]
                if color == (25, 90, 135):
                    presented = True
                    break
                assert process.poll() is None
                time.sleep(.02)
            assert presented, 'Portable did not present the expected source pixels'
            rect = wt.RECT()
            origin = wt.POINT()
            u.GetClientRect(hwnd, ctypes.byref(rect))
            u.ClientToScreen(hwnd, ctypes.byref(origin))
            ImageGrab.grab(bbox=(origin.x, origin.y, origin.x + rect.right, origin.y + rect.bottom)).save(run / 'portable.png')
            u.PostMessageW(hwnd, 0x10, 0, 0)
            assert process.wait(5) == 0, 'Portable shutdown failed'
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    zip_sha = hashlib.sha256(archive.read_bytes()).hexdigest()
    assert Path(str(archive) + '.sha256').read_text().split()[0] == zip_sha
    result = dict(archive=str(archive), exe_sha256=exe_sha, zip_sha256=zip_sha,
                  checked_files=len(manifest['files']), stale_file_excluded=True,
                  existing_portable_preserved=True, minimal_system_path_launch=True,
                  expected_image_presented=True, normal_shutdown=True, wrong_target_rejected=wrong_target_rejected)
    (run / 'results.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps(result), flush=True)
    print(run, flush=True)
finally:
    marker.unlink(missing_ok=True)
