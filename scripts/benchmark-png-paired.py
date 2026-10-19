"""Paired, hidden-process PNG benchmarks; warm OS cache, no application cache."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import statistics
import struct
import subprocess
import zlib

ROOT = Path(__file__).resolve().parents[1]


def run(exe, *args):
    result = subprocess.run([str(exe), *map(str, args)], cwd=ROOT, capture_output=True,
                            text=True, encoding='utf-8', check=True,
                            creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0))
    return json.loads(result.stdout)


def summary(values):
    ordered = sorted(values)
    return {'p50_us': statistics.median(values), 'p95_us': ordered[math.ceil(len(values) * .95) - 1]}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, default=ROOT / 'test-output/png-opt-20261019')
    parser.add_argument('--pairs', type=int, default=20)
    args = parser.parse_args()
    assert args.pairs > 0
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    before = out / 'before'
    after = ROOT / 'target/release/examples'
    files = [ROOT / 'test-output/readme-png-20261019' / name for name in ['png-control.png', 'alpha.png']]
    cases = []
    # All original processes are closed before measurement; build separately.
    for file in files:
        file.read_bytes()
        for kind, target in [('shared', None), ('single', 320), ('single', 1600), ('single', 0)]:
            def sample(version):
                folder = before if version == 'before' else after
                if kind == 'shared':
                    data = run(folder / 'source_work.exe', 'shared' if version == 'before' else 'inflight', 1, 512, 1, file)
                    sample = data['samples'][0]
                    item = sample['files'][0]
                    return {'us': sample['wall_us'], 'outputs': item['outputs'], 'codec_calls': item['codec_calls'],
                            'source_opens': item['source_opens'], 'read_bytes': item['read_bytes']}
                data = run(folder / 'profile.exe', file, target, 1, 1, 'native' if version == 'before' else 'inflight')
                sample = data['samples'][0]
                return {'us': sample['stages_us']['image-total'], 'stages_us': sample['stages_us']}
            for _ in range(2):
                sample('before'); sample('after')
            rows = []
            for pair in range(args.pairs):
                row = {version: sample(version) for version in (['before', 'after'] if pair % 2 == 0 else ['after', 'before'])}
                if kind == 'shared':
                    for old, new in zip(row['before']['outputs'], row['after']['outputs']):
                        assert {k: old[k] for k in ['target','width','height','hash']} == {k: new[k] for k in ['target','width','height','hash']}
                    assert row['after']['codec_calls'] == row['before']['codec_calls'] == 1
                rows.append(row)
            cases.append({'file': str(file.relative_to(ROOT)), 'sha256': digest(file), 'kind': kind,
                          'target': target, 'summary': {v: summary([r[v]['us'] for r in rows]) for v in ['before','after']}, 'pairs': rows})
    # Exercise each storage layout against the saved, unmodified shared binary.
    parity = []
    for file in sorted((ROOT / 'crates/decoder/tests/fixtures/png').glob('*.png')):
        old = run(before / 'source_work.exe', 'shared', 1, 512, 1, file)['samples'][0]['files'][0]
        new = run(after / 'source_work.exe', 'inflight', 1, 512, 1, file)['samples'][0]['files'][0]
        for a, b in zip(old['outputs'], new['outputs']):
            assert {k:a[k] for k in ['target','width','height','hash']} == {k:b[k] for k in ['target','width','height','hash']}, file
        parity.append({'file':file.name, 'sha256':digest(file), 'equal':True})
    # Many small ancillary chunks expose the old per-chunk unbuffered reads.
    file = out / 'many-text.png'
    data = files[0].read_bytes()
    chunks = bytearray()
    for index in range(256):
        text = f'field-{index}\0metadata value {index}'.encode()
        payload = b'tEXt' + text
        chunks.extend(struct.pack('>I', len(text)) + payload + struct.pack('>I', zlib.crc32(payload)))
    file.write_bytes(data[:-12] + chunks + data[-12:])
    info_cases = []
    for file in files + [file]:
        binaries = {'before':before/'png_info.exe', 'after':after/'png_info.exe'}
        for exe in binaries.values():run(exe, file, 2)
        rows = []
        for pair in range(args.pairs):
            row = {v:run(binaries[v], file, 1)['samples'][0] for v in (['before','after'] if pair%2==0 else ['after','before'])}
            assert row['before']['hash'] == row['after']['hash'], (file,row)
            rows.append(row)
        info_cases.append({'file':str(file.relative_to(ROOT)), 'sha256':digest(file),
                           'summary':{v:summary([r[v]['us'] for r in rows]) for v in binaries}, 'pairs':rows})
    artifacts = {str(p.relative_to(ROOT)):digest(p) for p in [before/'source_work.exe',before/'profile.exe',before/'png_info.exe',after/'source_work.exe',after/'profile.exe',after/'png_info.exe']}
    result = {'scope':'one worker, OS warm, no app cache; two warmups; alternating paired processes; synthetic derived fixtures; microseconds; no UI or physical SD measurements',
              'pairs_per_case':args.pairs, 'binaries':artifacts, 'decode_cases':cases, 'layout_parity':parity,'info_cases':info_cases}
    (out/'paired.json').write_text(json.dumps(result,indent=2),encoding='utf-8')
    print(json.dumps({k:result[k] for k in ['scope','pairs_per_case']}))
    for case in cases + info_cases:
        print(json.dumps({k:v for k,v in case.items() if k not in ['pairs','sha256']}))


if __name__ == '__main__':
    main()
