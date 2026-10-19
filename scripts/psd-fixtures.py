"""Generate deterministic multi-row PSD composites; no Photoshop dependency."""
import argparse
import json
from pathlib import Path
import struct
import zlib


def generate(root, width=4096, height=3072):
    root.mkdir(parents=True, exist_ok=True)
    paths = []
    for depth, channels in [(8, 4), (16, 3)]:
        rows = []
        predicted = []
        for channel in range(channels):
            samples = [(x * 257 + channel * 7919) % (1 << depth) for x in range(width)]
            pack = lambda values: bytes(values) if depth == 8 else struct.pack(f'>{width}H', *values)
            rows.append(pack(samples))
            predicted.append(pack([samples[0]] + [(b - a) % (1 << depth) for a, b in zip(samples, samples[1:])]))
        for compression in range(4):
            path = root / f'rgb-{depth}-{compression}.psd'
            with path.open('wb') as out:
                out.write(b'8BPS' + struct.pack('>H6xHIIHHIIIH', 1, channels, height, width, depth, 3, 0, 0, 0, compression))
                if compression == 1:
                    encoded = [b''.join(bytes([len(row[i:i+128]) - 1]) + row[i:i+128] for i in range(0, len(row), 128)) for row in rows]
                    for row in encoded:
                        out.write(struct.pack('>H', len(row)) * height)
                    for row in encoded:
                        for _ in range(height):
                            out.write(row)
                elif compression == 0:
                    for row in rows:
                        for _ in range(height):
                            out.write(row)
                else:
                    encoder = zlib.compressobj()
                    for row in predicted if compression == 3 else rows:
                        for _ in range(height):
                            out.write(encoder.compress(row))
                    out.write(encoder.flush())
            paths.append(path)
    return paths


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    print(json.dumps([str(path) for path in generate(args.output)]))
