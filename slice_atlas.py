#!/usr/bin/env python3
"""Slice a dumped sprite-atlas into individual images by alpha-connected-components.

The game's UI atlases (SingleModeScenarioLive_tex, Common_tex, ...) are ARBITRARILY packed — no grid
to cut (unlike Rank_tex, see curate_rank_icons.py). But sprites are separated by transparent gaps, so
we label alpha islands and crop each into its own PNG. Crops are UNNAMED (`<atlas>_007`) — for named
slicing we need the in-game Sprite.textureRects (planned as the icon_dump "Sprite pass").

Input:  a dumped atlas .rgba (raw RGBA, D3D vertically-flipped) or .png, from trackside-icons/_dump/
Output: <out>/<atlas>/<atlas>_<idx>_<w>x<h>.png  +  a <atlas>_sheet.png contact sheet for eyeballing.

  python slice_atlas.py SingleModeScenarioLive_tex_2048x2048.rgba   # a file (name resolved in _dump)
  python slice_atlas.py --all                                       # every *_tex atlas in _dump
  # tuning: --min 10 (min sprite px)  --max-frac 0.55 (skip near-full-atlas bg)  --close 0 (merge gaps)
"""
import argparse, glob, os, re

import numpy as np
from PIL import Image
from scipy import ndimage

GAME = r"G:\SteamLibrary\steamapps\common\UmamusumePrettyDerby"
DUMP = os.path.join(GAME, "trackside-icons", "_dump")
OUT_DEFAULT = os.path.join(GAME, "trackside-icons", "_sliced")
SCRATCH = os.environ.get("TEMP", ".")


def load_upright(path):
    """Load a dumped atlas as an upright RGBA image. .rgba is raw + D3D-flipped (like
    curate_rank_icons.py); .png is already upright."""
    if path.endswith(".rgba"):
        m = re.search(r"_(\d+)x(\d+)\.rgba$", path)
        w, h = int(m.group(1)), int(m.group(2))
        data = open(path, "rb").read()
        return Image.frombytes("RGBA", (w, h), data).transpose(Image.FLIP_TOP_BOTTOM)
    return Image.open(path).convert("RGBA")


def atlas_name(path):
    return re.sub(r"_(\d+)x(\d+)\.(rgba|png)$", "", os.path.basename(path))


def slice_atlas(path, out_root, min_dim=10, max_frac=0.55, close=0, alpha_thr=16, pad=1):
    img = load_upright(path)
    W, H = img.size
    alpha = np.asarray(img)[:, :, 3]
    mask = alpha > alpha_thr
    if close > 0:
        mask = ndimage.binary_closing(mask, structure=np.ones((close, close), bool))
    lbl, n = ndimage.label(mask, structure=np.ones((3, 3), int))  # 8-connectivity
    boxes = []
    for sl in ndimage.find_objects(lbl):
        if sl is None:
            continue
        ys, xs = sl
        w, h = xs.stop - xs.start, ys.stop - ys.start
        if w < min_dim or h < min_dim:
            continue                                   # dust / single-pixel noise
        if w * h > max_frac * W * H:
            continue                                   # near-full-atlas background panel
        boxes.append((ys.start, xs.start, w, h))
    # stable reading order: banded top→bottom, then left→right
    boxes.sort(key=lambda b: (b[0] // 24, b[1]))

    name = atlas_name(path)
    out = os.path.join(out_root, name)
    os.makedirs(out, exist_ok=True)
    for f in glob.glob(os.path.join(out, "*.png")):
        os.remove(f)                                   # clean re-run
    crops = []
    for idx, (y, x, w, h) in enumerate(boxes, 1):
        x0, y0 = max(0, x - pad), max(0, y - pad)
        x1, y1 = min(W, x + w + pad), min(H, y + h + pad)
        crop = img.crop((x0, y0, x1, y1))
        crop.save(os.path.join(out, f"{name}_{idx:03d}_{x1 - x0}x{y1 - y0}.png"))
        crops.append(crop)
    _contact_sheet(name, crops)
    print(f"{name}: {len(crops)} sprites -> {out}")
    return len(crops)


def _contact_sheet(name, crops, cell=96, cols=12):
    if not crops:
        return
    rows = (len(crops) + cols - 1) // cols
    sheet = Image.new("RGBA", (cols * cell, rows * cell), (24, 26, 30, 255))
    for i, c in enumerate(crops):
        t = c.copy()
        t.thumbnail((cell - 6, cell - 6))
        cx = (i % cols) * cell + (cell - t.width) // 2
        cy = (i // cols) * cell + (cell - t.height) // 2
        sheet.alpha_composite(t, (cx, cy))
    dst = os.path.join(SCRATCH, f"{name}_sheet.png")
    sheet.save(dst)
    print(f"  contact sheet: {dst}")


def resolve(arg):
    if os.path.isfile(arg):
        return arg
    for ext in (".rgba", ".png"):
        hits = glob.glob(os.path.join(DUMP, arg + "*" + ext)) or glob.glob(os.path.join(DUMP, arg))
        if hits:
            return hits[0]
    raise SystemExit(f"not found: {arg} (looked in {DUMP})")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("atlas", nargs="?", help="atlas file or name (resolved in _dump)")
    ap.add_argument("--all", action="store_true", help="slice every *_tex atlas in _dump")
    ap.add_argument("--out", default=OUT_DEFAULT)
    ap.add_argument("--min", type=int, default=10)
    ap.add_argument("--max-frac", type=float, default=0.55)
    ap.add_argument("--close", type=int, default=0)
    a = ap.parse_args()
    if a.all:
        targets = [p for p in glob.glob(os.path.join(DUMP, "*_tex_*.rgba"))]
        print(f"slicing {len(targets)} atlas(es) from {DUMP}")
    elif a.atlas:
        targets = [resolve(a.atlas)]
    else:
        raise SystemExit("give an atlas name/path, or --all")
    total = sum(slice_atlas(p, a.out, a.min, a.max_frac, a.close) for p in targets)
    print(f"done: {total} sprites total -> {a.out}")


if __name__ == "__main__":
    main()
