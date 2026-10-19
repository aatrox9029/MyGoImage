"""Capture one headless profile run with Windows lifetime peak working set."""
import json
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import time

import psutil

spec = importlib.util.spec_from_file_location('psd_peak', Path(__file__).with_name('psd-process-peak.py'))
peak_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(peak_module)


def capture(command):
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen(list(map(str, command)), stdout=stdout, stderr=stderr,
                                   creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0))
        monitored = psutil.Process(process.pid)
        sampled_peak = lifetime_peak = 0
        while process.poll() is None:
            try:
                memory = monitored.memory_info()
                sampled_peak = max(sampled_peak, memory.rss)
                lifetime_peak = max(lifetime_peak, getattr(memory, 'peak_wset', memory.rss))
            except psutil.NoSuchProcess:
                pass
            time.sleep(.002)
        stdout.seek(0)
        stderr.seek(0)
        if process.returncode:
            raise RuntimeError(stderr.read().decode('utf-8', errors='replace'))
        if os.name == 'nt':
            lifetime_peak = peak_module.peak_working_set(process)
        return {'rss_peak_bytes': sampled_peak, 'peak_working_set_bytes': lifetime_peak,
                'data': json.loads(stdout.read().decode('utf-8'))}
