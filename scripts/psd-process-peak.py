"""Read Windows lifetime peak working set using the retained process handle."""
import ctypes
from ctypes import wintypes


def peak_working_set(process):
    class Counters(ctypes.Structure):
        _fields_ = [('cb', wintypes.DWORD), ('page_faults', wintypes.DWORD)] + [
            (name, ctypes.c_size_t) for name in ['peak_working_set', 'working_set',
            'peak_paged_pool', 'paged_pool', 'peak_nonpaged_pool', 'nonpaged_pool',
            'pagefile', 'peak_pagefile']]

    api = ctypes.WinDLL('psapi', use_last_error=True).GetProcessMemoryInfo
    api.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
    api.restype = wintypes.BOOL
    counters = Counters()
    counters.cb = ctypes.sizeof(counters)
    # Popen retains this Windows handle until it is garbage collected. Query
    # after exit so a short final allocation cannot escape the polling interval.
    if not api(int(process._handle), ctypes.byref(counters), counters.cb):
        raise ctypes.WinError(ctypes.get_last_error())
    return counters.peak_working_set
