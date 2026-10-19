"""Deterministic PNG layout fixtures; stdlib only, filter 0, optional Adam7."""
from pathlib import Path
import struct
import zlib

OUT = Path(__file__).resolve().parents[1] / 'crates/decoder/tests/fixtures/png'

def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))

def make(name, depth, color, channels, interlace=False):
    width, height = 417, 259
    passes = [(0, 0, 1, 1)] if not interlace else [(0,0,8,8),(4,0,8,8),(0,4,4,8),(2,0,4,4),(0,2,2,4),(1,0,2,2),(0,1,1,2)]
    raw = bytearray()
    for x0, y0, dx, dy in passes:
        for y in range(y0, height, dy):
            raw.append(0)
            for x in range(x0, width, dx):
                for channel in range(channels):
                    value = (x * (17 + channel * 3) + y * (7 + channel * 11)) % (1 << depth)
                    raw.extend(value.to_bytes(depth // 8, 'big'))
    png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, depth, color, 0, 0, int(interlace)))
    if color == 3:
        png += chunk(b'PLTE', bytes(v for i in range(256) for v in (i, 255-i, (i*13)%256)))
        png += chunk(b'tRNS', bytes(range(256)))
    png += chunk(b'IDAT', zlib.compress(raw)) + chunk(b'tEXt', b'workflow\0{"nodes":[]}') + chunk(b'IEND', b'')
    (OUT / name).write_bytes(png)

if __name__ == '__main__':
    OUT.mkdir(parents=True, exist_ok=True)
    for args in [('rgb8.png',8,2,3),('rgba8.png',8,6,4),('gray8.png',8,0,1),('gray16.png',16,0,1),('gray-alpha.png',8,4,2),('rgb16.png',16,2,3),('rgba16.png',16,6,4),('palette.png',8,3,1),('adam7.png',8,6,4,True)]:
        make(*args)
