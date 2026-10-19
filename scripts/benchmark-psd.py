"""Compare headless profile executables with alternating order and OS-warm input."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import statistics

spec = importlib.util.spec_from_file_location('psd_capture', Path(__file__).with_name('psd-profile-capture.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def benchmark(before, after, files, output, rounds):
    result = {'note': 'OS-warm generated composites; RSS sampled every 2ms; Windows lifetime peak working set queried after exit via retained process handle; separate process per run.',
              'executables': {label: {'path': str(exe), 'sha256': hashlib.sha256(exe.read_bytes()).hexdigest()} for label, exe in [('before', before), ('after', after)]},
              'fixtures': [], 'samples': [], 'summary': []}
    for path in sorted(files.glob('*.psd')):
        result['fixtures'].append({'file': path.name, 'sha256': hashlib.sha256(path.read_bytes()).hexdigest(), 'bytes': path.stat().st_size})
        for index in range(rounds):
            for label, exe in [('before', before), ('after', after)][::1 if index % 2 == 0 else -1]:
                sample = module.capture([exe, path, 0, 1, 1, 'native'])
                sample.update(file=path.name, version=label, round=index)
                result['samples'].append(sample)
        summary = {'file': path.name}
        for label in ['before', 'after']:
            samples = [s for s in result['samples'] if s['file'] == path.name and s['version'] == label]
            summary[label] = {'codec_ms': statistics.median(s['data']['samples'][0]['stages_us']['image-codec'] / 1000 for s in samples),
                              'total_ms': statistics.median(s['data']['samples'][0]['stages_us']['image-total'] / 1000 for s in samples),
                              'rss_peak_bytes': statistics.median(s['rss_peak_bytes'] for s in samples),
                              'peak_working_set_bytes': statistics.median(s['peak_working_set_bytes'] for s in samples)}
        result['summary'].append(summary)
        output.write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps(result['summary'], indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--files', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=5)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error('rounds must be positive')
    benchmark(args.before.resolve(), args.after.resolve(), args.files, args.output, args.rounds)
