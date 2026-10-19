"""Isolated R05/R07/R08 measurements; build examples before running.
Requires psutil. Does not clear OS cache or measure UI/production lock contention.
"""
import argparse
import csv
import hashlib
import io
import json
from pathlib import Path
import subprocess
import time
import psutil

ROOT = Path(__file__).resolve().parents[1]

def capture(command):
    # Temporary output files avoid pipe backpressure while polling RSS.
    import tempfile
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        start = time.perf_counter()
        process = subprocess.Popen(list(map(str, command)), stdout=stdout, stderr=stderr)
        monitored = psutil.Process(process.pid)
        peak = 0
        while process.poll() is None:
            try:
                peak = max(peak, monitored.memory_info().rss)
            except psutil.NoSuchProcess:
                pass
            time.sleep(.002)
        stdout.seek(0)
        stderr.seek(0)
        text = stdout.read().decode('utf-8')
        error = stderr.read().decode('utf-8', errors='replace')
        if process.returncode:
            raise RuntimeError(f'{command}: {error}')
        return {'rss_peak_bytes': peak, 'process_ms': (time.perf_counter() - start) * 1000,
                'stdout': text}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--files', type=Path, nargs='+', required=True)
    parser.add_argument('--rounds', type=int, default=5)
    parser.add_argument('--cache-counts', type=int, nargs='*', default=[10000, 100000])
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error('rounds must be positive')
    args.output.mkdir(parents=True, exist_ok=False)
    executable = ROOT / 'target/release/examples/source_work.exe'
    maintenance = ROOT / 'target/release/examples/maintenance.exe'
    digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    result = {'fixtures': [{'path': str(p.resolve()), 'sha256': digest(p)} for p in args.files],
              'executables': {str(p): digest(p) for p in [executable, maintenance]},
              'rss_poll_ms': 2, 'source': [], 'cache': []}
    # Separate process per mode and alternating order; same per-job budget for
    # parallel/serial control, equal aggregate budget for shared/serial control.
    modes = [('serial', 1), ('shared', 1), ('serial', 2), ('parallel', 2), ('image-jpeg', 1)]
    for round_index in range(args.rounds):
        for mode, workers in modes[::1 if round_index % 2 == 0 else -1]:
            sample = capture([executable, mode, workers, 256, 1, *args.files])
            sample['data'] = json.loads(sample.pop('stdout'))
            sample['round'] = round_index
            result['source'].append(sample)
        (args.output / 'results.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    for count in args.cache_counts:
        sample = capture([maintenance, count, args.rounds])
        sample['rows'] = list(csv.DictReader(io.StringIO(sample.pop('stdout'))))
        result['cache'].append(sample)
        (args.output / 'results.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(args.output / 'results.json')

if __name__ == '__main__':
    main()
