"""Compare JPEG header I/O using two profile executables and identical inputs.

OS-warm, no application image cache; alternating process order. Decode timings
exclude fingerprint/dump work. RSS is sampled per process, not a memory limit.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import random
import subprocess
import time

from PIL import Image
import psutil

ROOT = Path(__file__).resolve().parents[1]


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def percentiles(values):
    values = sorted(values)
    return {f'p{int(q * 100)}': values[math.ceil(len(values) * q) - 1] for q in (.5, .95)}


def segment(marker, payload):
    return bytes([255, marker]) + (len(payload) + 2).to_bytes(2, 'big') + payload


def fixtures(output, source):
    output.mkdir()
    (output / 'reference.jpg').write_bytes(source.read_bytes())
    with Image.open(source) as image:
        rgb = image.convert('RGB')
        icc = image.info.get('icc_profile')
    assert icc, 'Reference photo must contain ICC'
    rgb.thumbnail((1800, 1800))
    rgb.info.clear()
    rgb.save(output / 'progressive.jpg', quality=90, progressive=True, icc_profile=icc)
    rgb.convert('L').save(output / 'gray.jpg', quality=90)
    noise = Image.frombytes('RGB', (1800, 1200), random.Random(42).randbytes(1800 * 1200 * 3))
    noise.save(output / 'detail.jpg', quality=95, subsampling=0)
    rgb.save(output / 'metadata-after-sof.jpg', quality=90)
    path = output / 'metadata-after-sof.jpg'
    data = path.read_bytes()
    position = 2
    while data[position + 1] not in (0xc0, 0xc2):
        position += 2 + int.from_bytes(data[position + 2:position + 4], 'big')
    position += 2 + int.from_bytes(data[position + 2:position + 4], 'big')
    exif = b'Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0'
    midpoint = len(icc) // 2
    # Metadata after SOF is legal. Reverse chunk order exercises ICC sequencing.
    metadata = segment(0xe1, exif)
    metadata += segment(0xe2, b'ICC_PROFILE\0\x02\x02' + icc[midpoint:])
    metadata += segment(0xe2, b'ICC_PROFILE\0\x01\x02' + icc[:midpoint])
    path.write_bytes(data[:position] + metadata + data[position:])
    rgb.thumbnail((900, 900))
    rgb.save(output / 'png-control.png')


def run(exe, path, target, iterations, output):
    path.read_bytes()  # Explicit OS-cache warm-up, not hardware-cold storage.
    command = [str(exe), str(path), str(target), str(iterations), '1', 'native']
    with output.open('w', encoding='utf-8') as stdout:
        process = subprocess.Popen(command, stdout=stdout, stderr=subprocess.PIPE)
        observed = psutil.Process(process.pid)
        peak = 0
        while process.poll() is None:
            try:
                peak = max(peak, observed.memory_info().rss)
            except psutil.Error:
                pass
            time.sleep(.002)
        _, stderr = process.communicate()
    if process.returncode:
        raise RuntimeError(stderr.decode('utf-8', errors='replace'))
    result = json.loads(output.read_text(encoding='utf-8'))
    assert len(result['samples']) == iterations
    for sample in result['samples']:
        stages = sample['stages_us']
        assert sum(value for key, value in stages.items() if key != 'image-total') == stages['image-total']
    result['peak_rss_bytes'] = peak
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--source-photo', type=Path, default=ROOT / 'test-output/reference-photo/iss036e030926.jpg')
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--iterations', type=int, default=10)
    args = parser.parse_args()
    if args.rounds < 1 or args.iterations < 1:
        parser.error('rounds and iterations must be positive')
    args.output.mkdir(parents=True, exist_ok=False)
    photos = args.output / 'photos'
    fixtures(photos, args.source_photo)
    executables = {'before': args.before.resolve(), 'after': args.after.resolve()}
    manifest = []
    rows = []
    for path in sorted(photos.iterdir()):
        with Image.open(path) as image:
            manifest.append(dict(name=path.name, bytes=path.stat().st_size, sha256=sha256(path), dimensions=image.size))
        for target in (320, 1600):
            # Independently compare complete RGBA bytes, not only fingerprints.
            dumps = []
            for label, exe in executables.items():
                dump = args.output / f'{path.stem}-{target}-{label}.rgba'
                env = dict(os.environ, MYGO_PROFILE_RGBA_OUTPUT=str(dump.resolve()))
                subprocess.run([str(exe), str(path.resolve()), str(target), '1', '1', 'native'],
                               env=env, check=True, capture_output=True)
                dumps.append(dump)
            assert dumps[0].read_bytes() == dumps[1].read_bytes(), (path, target, 'pixel mismatch')
            runs = {'before': [], 'after': []}
            for round_index in range(args.rounds):
                order = ('before', 'after') if round_index % 2 == 0 else ('after', 'before')
                for label in order:
                    raw = args.output / f'{path.stem}-{target}-{round_index}-{label}.json'
                    runs[label].append(run(executables[label], path.resolve(), target, args.iterations, raw))
            all_samples = {label: [s for run_result in results for s in run_result['samples']]
                           for label, results in runs.items()}
            fields = ('width', 'height', 'original_width', 'original_height', 'orientation', 'rgba_fnv1a64')
            expected = tuple(all_samples['before'][0][key] for key in fields)
            assert all(tuple(s[key] for key in fields) == expected for samples in all_samples.values() for s in samples)
            if path.stem == 'metadata-after-sof':
                assert all_samples['after'][0]['orientation'] == 6
            summary = {}
            for label, samples in all_samples.items():
                summary[label] = dict(
                    stages_us={key: percentiles([s['stages_us'][key] for s in samples]) for key in samples[0]['stages_us']},
                    read_bytes=percentiles([s['read_bytes'] for s in samples]),
                    process_peak_rss_bytes=percentiles([r['peak_rss_bytes'] for r in runs[label]]),
                )
            rows.append(dict(name=path.name, target=target, samples_per_version=len(all_samples['before']),
                             pixels_equal=True, rgba_sha256=sha256(dumps[0]), summary=summary))
            print(f"{path.name} target={target}: {summary['before']['stages_us']['image-total']['p50']} -> "
                  f"{summary['after']['stages_us']['image-total']['p50']} us", flush=True)
    report = dict(scope=__doc__, rounds=args.rounds, iterations=args.iterations, os_cache_flushed=False,
                  exe_sha256={label: sha256(exe) for label, exe in executables.items()}, manifest=manifest, rows=rows)
    (args.output / 'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')


if __name__ == '__main__':
    main()
