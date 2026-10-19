"""Verify every embedded Windows icon frame without launching the application."""
import argparse
import ctypes
from ctypes import wintypes as wt
import json
from pathlib import Path
import struct

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('exe', type=Path)
parser.add_argument('--ico', type=Path, default=Path(__file__).resolve().parents[1] / 'assets/icon.ico')
args = parser.parse_args()
kernel = ctypes.WinDLL('kernel32', use_last_error=True)
kernel.LoadLibraryExW.argtypes = [wt.LPCWSTR, wt.HANDLE, wt.DWORD]
kernel.LoadLibraryExW.restype = wt.HMODULE
kernel.FindResourceW.argtypes = [wt.HMODULE, wt.LPCVOID, wt.LPCVOID]
kernel.FindResourceW.restype = wt.HANDLE
kernel.LoadResource.argtypes = [wt.HMODULE, wt.HANDLE]
kernel.LoadResource.restype = wt.HANDLE
kernel.LockResource.argtypes = [wt.HANDLE]
kernel.LockResource.restype = wt.LPVOID
kernel.SizeofResource.argtypes = [wt.HMODULE, wt.HANDLE]
kernel.SizeofResource.restype = wt.DWORD
kernel.FreeLibrary.argtypes = [wt.HMODULE]
module = kernel.LoadLibraryExW(str(args.exe.resolve()), None, 2)
if not module:
    raise ctypes.WinError(ctypes.get_last_error())


def resource(kind, identifier):
    found = kernel.FindResourceW(module, identifier, kind)
    if not found:
        raise ctypes.WinError(ctypes.get_last_error())
    loaded = kernel.LoadResource(module, found)
    pointer = kernel.LockResource(loaded)
    assert pointer, f'Cannot load icon resource {kind}/{identifier}'
    return ctypes.string_at(pointer, kernel.SizeofResource(module, found))


try:
    ico = args.ico.read_bytes()
    group = resource(14, 1)
    assert group[:6] == ico[:6], 'ICO and executable frame counts differ'
    count = struct.unpack_from('<H', ico, 4)[0]
    sizes = []
    for i in range(count):
        entry = ico[6 + 16 * i:22 + 16 * i]
        embedded = group[6 + 14 * i:20 + 14 * i]
        # RC normalizes ICO planes (often zero) from the encoded image header.
        assert entry[:4] == embedded[:4] and entry[6:12] == embedded[6:12], (
            f'Frame {i} metadata differs: {entry.hex()} / {embedded.hex()}'
        )
        length, offset = struct.unpack_from('<II', entry, 8)
        identifier = struct.unpack_from('<H', embedded, 12)[0]
        assert resource(3, identifier) == ico[offset:offset + length], f'Frame {i} pixels differ'
        sizes.append([entry[0] or 256, entry[1] or 256])
    print(json.dumps(dict(exe=str(args.exe), frames=count, sizes=sizes, verified=True)))
finally:
    kernel.FreeLibrary(module)
