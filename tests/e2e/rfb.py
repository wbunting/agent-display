"""Minimal headless VNC (RFB 3.8, no auth) client for the e2e checks: stands
in for a human's vncviewer without opening a window anywhere.

usage: python3 -I rfb.py unix:/path/to.sock|tcp:HOST:PORT OUT.png
           [--click X Y] [--type TEXT] [--hold SECONDS]

Writes OUT.png (first full frame) and OUT-after.png (after the input), and
prints one JSON line: {"name", "width", "height", "frames"}.
"""
import json
import socket
import struct
import sys
import time
import zlib


def png(path, w, h, fb):
    rows = []
    for y in range(h):
        row = fb[y * w * 4:(y + 1) * w * 4]
        rgb = bytearray(w * 3)
        rgb[0::3], rgb[1::3], rgb[2::3] = row[2::4], row[1::4], row[0::4]  # BGRX -> RGB
        rows.append(b'\0' + bytes(rgb))

    def chunk(t, d):
        return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d) & 0xffffffff)
    with open(path, 'wb') as f:
        f.write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0))
                + chunk(b'IDAT', zlib.compress(b''.join(rows))) + chunk(b'IEND', b''))


class Rfb:
    def __init__(self, target):
        kind, _, addr = target.partition(':')
        if kind == 'unix':
            self.s = socket.socket(socket.AF_UNIX)
            self.s.connect(addr)
        else:
            host, _, port = addr.rpartition(':')
            self.s = socket.create_connection((host, int(port)))
        self.recvn(12)
        self.s.sendall(b'RFB 003.008\n')
        types = self.recvn(self.recvn(1)[0])
        if 1 not in types:
            raise SystemExit(f'server wants auth: {list(types)}')
        self.s.sendall(b'\x01')
        if struct.unpack('>I', self.recvn(4))[0] != 0:
            raise SystemExit('security handshake failed')
        self.s.sendall(b'\x01')  # shared
        self.w, self.h = struct.unpack('>HH', self.recvn(4))
        self.recvn(16)
        self.name = self.recvn(struct.unpack('>I', self.recvn(4))[0]).decode()
        # 32 bpp, little-endian BGRX; Raw encoding + DesktopSize.
        self.s.sendall(struct.pack('>BxxxBBBBHHHBBBxxx', 0, 32, 24, 0, 1, 255, 255, 255, 16, 8, 0))
        self.s.sendall(struct.pack('>BxH', 2, 2) + struct.pack('>ii', 0, -223))
        self.fb = bytearray(self.w * self.h * 4)
        self.frames = 0
        self.request(incremental=0)

    def recvn(self, n):
        b = b''
        while len(b) < n:
            c = self.s.recv(n - len(b))
            if not c:
                raise EOFError('server closed the connection')
            b += c
        return b

    def request(self, incremental=1):
        self.s.sendall(struct.pack('>BBHHHH', 3, incremental, 0, 0, self.w, self.h))

    def pump(self, secs):
        end = time.time() + secs
        while time.time() < end:
            self.s.settimeout(max(0.01, min(0.3, end - time.time())))
            try:
                t = self.recvn(1)[0]
            except socket.timeout:
                continue
            finally:
                self.s.settimeout(None)
            if t == 0:
                self.recvn(1)
                for _ in range(struct.unpack('>H', self.recvn(2))[0]):
                    x, y, rw, rh, enc = struct.unpack('>HHHHi', self.recvn(12))
                    if enc == 0:
                        data = self.recvn(rw * rh * 4)
                        for r in range(rh):
                            o = ((y + r) * self.w + x) * 4
                            self.fb[o:o + rw * 4] = data[r * rw * 4:(r + 1) * rw * 4]
                    elif enc == -223:
                        self.w, self.h = rw, rh
                        self.fb = bytearray(rw * rh * 4)
                self.frames += 1
                self.request()
            elif t == 2:
                pass
            elif t == 3:
                self.recvn(3)
                self.recvn(struct.unpack('>I', self.recvn(4))[0])
            else:
                raise SystemExit(f'unexpected server message {t}')

    def pointer(self, x, y, mask):
        self.s.sendall(struct.pack('>BBHH', 5, mask, x, y))
        self.pump(0.1)

    def key(self, keysym, down):
        self.s.sendall(struct.pack('>BBxxI', 4, 1 if down else 0, keysym))
        self.pump(0.03)


def main(a):
    c = Rfb(a[0])
    c.pump(2.0)  # the first frames (the very first can be blank)
    png(a[1], c.w, c.h, c.fb)
    if '--click' in a:
        i = a.index('--click')
        x, y = int(a[i + 1]), int(a[i + 2])
        for mask in (0, 0, 1, 0):
            c.pointer(x, y, mask)
    if '--type' in a:
        for ch in a[a.index('--type') + 1]:
            c.key(ord(ch), True)
            c.key(ord(ch), False)
    c.pump(float(a[a.index('--hold') + 1]) if '--hold' in a else 0.5)
    png(a[1].replace('.png', '-after.png'), c.w, c.h, c.fb)
    print(json.dumps({'name': c.name, 'width': c.w, 'height': c.h, 'frames': c.frames}))
    c.s.close()


if __name__ == '__main__':
    main(sys.argv[1:])
