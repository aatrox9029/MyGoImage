"""Alternating paired native-window benchmarks, preserving every JSON sample."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import shutil
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--before', type=Path, required=True)
parser.add_argument('--after', type=Path, default=root/'target/release/mygoimage.exe')
parser.add_argument('--source-photo', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--pairs', type=int, default=5)
a = parser.parse_args()
assert a.pairs > 0
out = a.output.resolve()
out.mkdir(parents=True, exist_ok=False)
records = []
for pair in range(a.pairs):
    order = ['before', 'after'] if pair % 2 == 0 else ['after', 'before']
    for variant in order:
        work = out/variant
        work.mkdir(exist_ok=True)
        with (out/f'{pair}-{variant}.stdout.log').open('w') as log:
            subprocess.run([sys.executable, str(root/'scripts/benchmark-viewer.py'),
                            '--exe', str(getattr(a, variant).resolve()), '--output', str(work),
                            '--runs', '1', '--source-photo', str(a.source_photo.resolve())],
                           check=True, stdout=log)
        result = json.loads((work/'results.json').read_text(encoding='utf-8'))
        (out/f'{pair}-{variant}.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
        for screenshot in work.glob('*.png'):
            shutil.copy2(screenshot, out/f'{pair}-{variant}-{screenshot.name}')
        for trace in work.glob('*.diagnostics.json'):
            shutil.copy2(trace, out/f'{pair}-{variant}-{trace.name}')
        records.append(dict(pair=pair+1, variant=variant, result=result))
        print(json.dumps(dict(pair=pair+1, variant=variant, summary=result['summary_ms'])), flush=True)

def percentiles(values):
    values = sorted(values)
    return dict(n=len(values), **{f'p{int(q*100)}': values[math.ceil((len(values)-1)*q)] for q in [.5, .95]})
summary = {}
for variant in ['before', 'after']:
    runs = [run for record in records if record['variant'] == variant for run in record['result']['runs']]
    samples = [sample for run in runs for sample in run['samples']]
    summary[variant] = {scenario: percentiles([s['ms'] for s in samples if s['scenario'] == scenario])
                        for scenario in sorted({s['scenario'] for s in samples})}
    summary[variant]['rss_bytes'] = percentiles([run['peak_rss_bytes'] for run in runs])
report = dict(scope='Alternating before/after order; each pair contains fresh cold application cache and warm restart; OS cache not cleared; screenshot polling overhead included; GPU not measured',
              before_sha256=hashlib.sha256(a.before.read_bytes()).hexdigest(),
              after_sha256=hashlib.sha256(a.after.read_bytes()).hexdigest(), records=records, summary=summary)
(out/'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
print(json.dumps(summary), flush=True)
