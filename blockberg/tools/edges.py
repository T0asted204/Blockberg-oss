#!/usr/bin/env python3
"""Print colour transitions along a column (x=N) or row (y=N) in the mockup vs the render.
   tools/edges.py x 300 [from to] [shot.png]"""
import sys, subprocess
from PIL import Image
axis, pos = sys.argv[1], int(sys.argv[2])
lo, hi = (int(sys.argv[3]), int(sys.argv[4])) if len(sys.argv) > 4 else (0, 10**9)
shot = sys.argv[5] if len(sys.argv) > 5 else '/tmp/blockberg-compare/shot.png'
for name, path in [('mock', __file__.rsplit('/',1)[0] + '/reference/mockup.png'), ('ours', shot)]:
    im = Image.open(path).convert('RGB'); W, H = im.size
    n = H if axis == 'x' else W
    out, prev = [], None
    for i in range(max(lo,0), min(hi, n)):
        c = im.getpixel((pos, i) if axis == 'x' else (i, pos))
        if prev is None or sum(abs(a-b) for a, b in zip(c, prev)) > 60:
            out.append(f'{i}:{"%02x%02x%02x" % c}')
        prev = c
    print(name, ' '.join(out))
