"""Compare CPU resizers first, then GPU CPU-to-CPU resize end-to-end.

Output directories must be new. Inputs are read-only and OS-cache warm.
Raw timings, output PNGs, composite quality comparisons and process RSS remain
available for inspection. Lanczos3 is a reference filter, not ground truth.
"""
import argparse
import datetime
import hashlib
import json
import math
import platform
from pathlib import Path
import shutil
import subprocess
import time

import numpy as np
import psutil
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]


def percentiles(values):
    values = sorted(values)
    return {f'p{int(q*100)}': values[math.ceil((len(values)-1)*q)] for q in (.5, .95)}


def quality(actual, reference):
    with Image.open(actual) as image:
        a = np.asarray(image.convert('RGBA'), dtype=np.float32)
    with Image.open(reference) as image:
        b = np.asarray(image.convert('RGBA'), dtype=np.float32)
    assert a.shape == b.shape
    # Hidden RGB in transparent pixels must not distort the quality metric.
    differences = []
    for background in (0., 255.):
        ac = a[:, :, :3] * (a[:, :, 3:] / 255.) + background * (1-a[:, :, 3:] / 255.)
        bc = b[:, :, :3] * (b[:, :, 3:] / 255.) + background * (1-b[:, :, 3:] / 255.)
        differences.append(ac-bc)
    mse = sum(float(np.mean(d*d)) for d in differences) / 2
    return dict(composited_mae=sum(float(np.mean(np.abs(d))) for d in differences) / 2, composited_psnr_db=10*math.log10(255**2/mse) if mse else None, alpha_max_error=float(np.max(np.abs(a[:, :, 3]-b[:, :, 3]))))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--iterations', type=int, default=12)
    parser.add_argument('--source-photo', type=Path, default=ROOT/'test-output/reference-photo/iss036e030926.jpg')
    args = parser.parse_args()
    assert args.iterations >= 4
    args.output.mkdir(parents=True, exist_ok=False)
    photos = args.output / 'photos'
    photos.mkdir()
    shutil.copyfile(args.source_photo, photos/'reference.jpg')
    with Image.open(args.source_photo) as source:
        icc = source.info.get('icc_profile')
        assert icc, 'Reference photo needs ICC for the color-managed workload'
        rgb = source.convert('RGB')
        preview = rgb.copy()
        preview.thumbnail((1800, 1800))
        preview.info.clear()
        preview.save(photos/'preview.png')
        preview.save(photos/'preview-icc.png', icc_profile=icc)
        rgb.save(photos/'large.tiff', compression='raw', icc_profile=icc)
    yy, xx = np.indices((1536, 2048))
    checker = np.where(((xx//3 + yy//3) % 2)[:, :, None], np.array([240, 220, 15]), np.array([10, 30, 210])).astype('uint8')
    checker[(xx % 101 == 0) | (yy % 97 == 0)] = 0
    Image.fromarray(checker, 'RGB').save(photos/'checker.png')
    alpha = np.zeros((1536, 2048, 4), dtype='uint8')
    alpha[:, :, :3] = [255, 0, 255]
    visible = xx + yy//3 > 1250
    alpha[visible, :3] = [20, 210, 50]
    alpha[:, :, 3] = np.where(visible, np.minimum((xx+yy//3-1250)*3, 255), 0)
    Image.fromarray(alpha, 'RGBA').save(photos/'alpha.png')
    gray = ((xx*65535//2047 + yy*127) % 65536).astype('uint16')
    Image.fromarray(gray).save(photos/'gray16.png')
    with Image.open(photos/'reference.jpg') as source:
        icc = source.info.get('icc_profile')
        oriented = source.convert('RGB').resize((2400, 1600))
        exif = Image.Exif()
        exif[274] = 6
        oriented.save(photos/'orientation6.jpg', exif=exif, icc_profile=icc, quality=95)
    manifest = []
    for path in sorted(photos.iterdir()):
        with Image.open(path) as image:
            manifest.append(dict(name=path.name, sha256=hashlib.sha256(path.read_bytes()).hexdigest(), dimensions=image.size, mode=image.mode, icc=bool(image.info.get('icc_profile'))))
    exe = ROOT/'target/release/examples/resize_compare.exe'
    phases = {}
    # Sequential phases: inspect CPU numbers before GPU context is initialized.
    for phase in ('cpu', 'gpu'):
        destination = args.output/phase
        with (args.output/f'{phase}.log').open('w', encoding='utf-8') as log:
            process = subprocess.Popen([str(exe), str(photos.resolve()), str(destination.resolve()), phase, str(args.iterations)], stdout=log, stderr=log)
            observed = psutil.Process(process.pid)
            peak = 0
            started = time.monotonic()
            while process.poll() is None:
                try:
                    peak = max(peak, observed.memory_info().rss)
                except psutil.Error:
                    break
                if time.monotonic()-started > 600:
                    process.kill()
                    raise TimeoutError(phase)
                time.sleep(.01)
        assert process.wait() == 0, (args.output/f'{phase}.log').read_text(encoding='utf-8')
        report = json.loads((destination/'raw.json').read_text(encoding='utf-8'))
        report['process_peak_rss_bytes'] = peak
        for case in report['cases']:
            metadata = {(tuple(row['output_dimensions']), tuple(row['original_dimensions']), row['orientation']) for row in case['warmups']}
            assert len(metadata) == 1, 'Resize changed dimensions or EXIF interpretation'
            if phase == 'gpu':
                original = next(c for c in phases['cpu']['cases'] if (c['name'], c['target']) == (case['name'], case['target']))
                reference_metadata = {(tuple(row['output_dimensions']), tuple(row['original_dimensions']), row['orientation']) for row in original['warmups']}
                assert metadata == reference_metadata
            summaries = {}
            for method in sorted({row['method'] for row in case['samples']}):
                rows = [row['stages_us'] for row in case['samples'] if row['method'] == method]
                assert len(rows) == args.iterations
                for row in rows:
                    assert sum(value for event, value in row.items() if event != 'image-total') == row['image-total']
                stem = f"{case['name']}-{case['target']}"
                actual = destination/f'{stem}-{method}.png'
                reference = args.output/'cpu'/f'{stem}-fir-lanczos3.png'
                summaries[method] = dict(resize_us=percentiles([r['image-resize'] for r in rows]), total_us=percentiles([r['image-total'] for r in rows]), vs_lanczos3=quality(actual, reference))
                if method == 'gpu-triangle':
                    summaries[method]['vs_cpu_triangle'] = quality(actual, destination/f'{stem}-fir-triangle.png')
            case['summary'] = summaries
            print(phase, case['name'], case['target'], {method: round(row['total_us']['p50']/1000, 2) for method, row in summaries.items()}, flush=True)
        phases[phase] = report
        # Persist CPU results even if GPU initialization or execution fails.
        (args.output/f'{phase}-results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
    report = dict(date=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(), platform=platform.platform(), exe_sha256=hashlib.sha256(exe.read_bytes()).hexdigest(), os_cache_flushed=False, image_size_limit_preset=1, manifest=manifest, phases=phases, notes='No app image cache or Slint presentation. Same-process rotating order; one untimed warmup per method/case retained separately. Warm-context GPU includes RGBA8 conversion, allocations, upload, antialiased separable triangle, GPU completion/readback and ICC. GPU16 path quantizes before resizing, unlike CPU16; quality reference is a filter, not ground truth. RSS is process-wide, GPU memory is not measured.')
    (args.output/'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')


if __name__ == '__main__':
    main()
