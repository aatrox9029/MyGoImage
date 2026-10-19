"""OS-warm decode stage measurements and isolated 1/2-worker comparison.

Uses production EXIF/resize/ICC processing; does not change viewer scheduling.
The alternative image-crate JPEG codec does not use native reduced-resolution
decoding, so preview comparisons include that extra work.
"""
import argparse
import datetime
import hashlib
import json
import math
import platform
from pathlib import Path
import subprocess
import time

import psutil
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]


def percentiles(values):
    values = sorted(values)
    return {f'p{int(q * 100)}': values[math.ceil((len(values) - 1) * q)] for q in (.5, .95)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--source-photo', type=Path, default=ROOT / 'test-output/reference-photo/iss036e030926.jpg')
    parser.add_argument('--iterations', type=int, default=16)
    args = parser.parse_args()
    if args.iterations < 2:
        parser.error('--iterations must be >= 2')
    args.output.mkdir(parents=True, exist_ok=False)
    photos = args.output / 'photos'
    photos.mkdir()
    source = Image.open(args.source_photo)
    icc = source.info.get('icc_profile')
    assert icc, 'Reference photo must have an ICC profile for the ICC workload'
    rgb = source.convert('RGB')
    (photos / 'reference.jpg').write_bytes(args.source_photo.read_bytes())
    preview = rgb.copy()
    preview.thumbnail((1800, 1800))
    preview.info.clear()  # Pillow otherwise carries the source ICC into both PNGs.
    preview.save(photos / 'preview.png')
    preview.save(photos / 'preview-icc.png', icc_profile=icc)
    rgb.save(photos / 'large.tiff', compression='raw', icc_profile=icc)
    exe = ROOT / 'target/release/examples/profile.exe'
    manifest = []
    results = []
    for path in sorted(photos.iterdir()):
        data = path.read_bytes()  # Explicit OS-cache warm-up; not hardware-cold.
        with Image.open(path) as image:
            manifest.append(dict(name=path.name, bytes=len(data), sha256=hashlib.sha256(data).hexdigest(), dimensions=image.size, icc=bool(image.info.get('icc_profile'))))
        for target in (320, 1600):
            for backend in (('native', 'image') if path.suffix == '.jpg' else ('native',)):
                for workers in (1, 2):
                    command = [str(exe), str(path.resolve()), str(target), str(args.iterations), str(workers), backend]
                    # Warm executable/data OS pages in a separate process.
                    # The measured process still includes its first decode.
                    subprocess.run(command[:3] + ['2'] + command[4:], check=True, capture_output=True, text=True, encoding='utf-8')
                    raw_path = args.output / f'{path.stem}-{target}-{backend}-{workers}.json'
                    raw_file = raw_path.open('w', encoding='utf-8')
                    process = subprocess.Popen(command, stdout=raw_file, stderr=subprocess.PIPE, text=True, encoding='utf-8')
                    peak = 0
                    observed = psutil.Process(process.pid)
                    while process.poll() is None:
                        try:
                            peak = max(peak, observed.memory_info().rss)
                        except psutil.Error:
                            break
                        time.sleep(.005)
                    _, stderr = process.communicate()
                    raw_file.close()
                    if process.returncode:
                        raise RuntimeError(stderr)
                    result = json.loads(raw_path.read_text(encoding='utf-8'))
                    assert len(result['samples']) == args.iterations
                    for sample in result['samples']:
                        stages = sample['stages_us']
                        assert sum(value for event, value in stages.items() if event != 'image-total') == stages['image-total']
                    result.update(name=path.name, backend=backend, peak_rss_bytes=peak)
                    result['summary_us'] = {stage: percentiles([s['stages_us'][stage] for s in result['samples']]) for stage in result['samples'][0]['stages_us']}
                    totals = {stage: sum(s['stages_us'][stage] for s in result['samples']) for stage in result['samples'][0]['stages_us']}
                    result['share_percent'] = {stage: value / totals['image-total'] * 100 for stage, value in totals.items() if stage != 'image-total'}
                    result['images_per_second'] = args.iterations * 1_000_000 / result['wall_us']
                    results.append(result)
                    print(f"{path.name} target={target} {backend} workers={workers}: p50={result['summary_us']['image-total']['p50']/1000:.2f}ms; {result['images_per_second']:.1f} images/s", flush=True)
    report = dict(date=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(), platform=platform.platform(), cpu=platform.processor(), exe_sha256=hashlib.sha256(exe.read_bytes()).hexdigest(), image_size_limit_preset=1, os_cache_flushed=False, scope='OS-warm, no application image cache; 24MP preset (384MB per-job working budget); generated derivatives plus original reference; separate processes per case; workers repeat one input, not mixed files or interactive latency; RSS sampled at 5ms; exclusive wall times, not CPU usage or physical disk latency', manifest=manifest, results=results)
    (args.output / 'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')


if __name__ == '__main__':
    main()
