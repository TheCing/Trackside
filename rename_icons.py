#!/usr/bin/env python3
"""Curate the named sprite slices into a tidy, browsable icon library.

Input:  trackside-icons/_sliced/<atlas>/<raw_name>.png   (from `slice_atlas.py --manifest`)
Output: trackside-icons/_curated/<category>/<clean_name>.png   +   _curated/index.html gallery

Cleans the game's asset names (strips the utx_ / tx_uTex_fl_ prefixes and the _00 / _0_C boilerplate),
buckets each sprite by kind (buttons / frames / text / icons / numbers / gauges / backgrounds / misc),
de-dups name collisions, and writes a self-navigating HTML contact sheet you open in a browser to see
every icon with both its cleaned name and the original asset name.

  python rename_icons.py            # curate everything in _sliced/
  python rename_icons.py --atlas SingleModeScenarioLive_tex   # just one atlas
"""
import argparse, glob, html, os, re, shutil

import numpy as np
from PIL import Image
from scipy import ndimage

GAME = r"G:\SteamLibrary\steamapps\common\UmamusumePrettyDerby"
DUMP = os.path.join(GAME, "trackside-icons", "_dump")
SLICED = os.path.join(GAME, "trackside-icons", "_sliced")
CURATED = os.path.join(GAME, "trackside-icons", "_curated")

# token -> folder/category, in priority order (first matching token in the name wins)
CAT_TOKENS = [("btn", "buttons"), ("frm", "frames"), ("txt", "text"), ("num", "numbers"),
              ("gau", "gauges"), ("chr", "characters"), ("chara", "characters"),
              ("ico", "icons"), ("icon", "icons"), ("badge", "icons"), ("emblem", "icons"),
              ("skill", "skills"), ("support", "support"), ("bg", "backgrounds"), ("img", "images")]

# Standalone _dump textures that are NOT icons (backgrounds, 3D/character sheets, splash, video masks).
NON_ICON = re.compile(r"^(bg_|vertical_bg|horizontal_bg|tex_|dress_|splash|logo|cri|mask_|movie|cutt)", re.I)
ICON_MAXDIM = 1024   # drop backgrounds / big sheets; keep icon/frame-sized single textures


def clean_name(name):
    """utx_btn_play_main_s_00 -> btn_play_main_s ;  ..._badge_performance00_0_C -> ..._badge_performance"""
    n = name.lower()
    n = re.sub(r"_\d+x\d+$", "", n)     # strip the _WxH size suffix carried by standalone _dump files
    for p in ("tx_utex_fl_", "tx_utex_", "utx_", "utex_"):
        if n.startswith(p):
            n = n[len(p):]
            break
    n = re.sub(r"(\d+)?_0_c$", "", n)   # _0_C / 00_0_C trailing variant marker
    n = re.sub(r"_00$", "", n)          # trailing _00
    return n or name.lower()


def category(clean):
    toks = clean.split("_")
    for key, folder in CAT_TOKENS:
        if key in toks:
            return folder
    return "misc"


def _dims(fname):
    m = re.search(r"_(\d+)x(\d+)\.png$", fname)
    return (int(m.group(1)), int(m.group(2))) if m else (0, 0)


def is_standalone_icon(fname):
    """A _dump texture that's a usable individual icon (not an atlas, render-texture, background or
    character/3D sheet)."""
    if "_tex_" in fname or fname.startswith("RenderTexture") or "ImageEffects" in fname:
        return False
    base = re.sub(r"_\d+x\d+\.png$", "", fname)
    if NON_ICON.match(base):
        return False
    w, h = _dims(fname)
    return 0 < max(w, h) <= ICON_MAXDIM


def cc_boxes(img, min_dim=12, max_frac=0.6, alpha_thr=16):
    """Bounding boxes of alpha-connected islands (upright coords) — used to tell a packed sheet
    (many islands) from a single icon, and to cut the sheet up."""
    a = np.asarray(img)[:, :, 3] > alpha_thr
    lbl, _ = ndimage.label(a, structure=np.ones((3, 3), int))
    W, H = img.size
    out = []
    for sl in ndimage.find_objects(lbl):
        if sl is None:
            continue
        ys, xs = sl
        w, h = xs.stop - xs.start, ys.stop - ys.start
        if w < min_dim or h < min_dim or w * h > max_frac * W * H:
            continue
        out.append((xs.start, ys.start, w, h))
    return out


def curate(atlas_filter=None):
    if os.path.isdir(CURATED):
        shutil.rmtree(CURATED)
    os.makedirs(CURATED, exist_ok=True)
    # category -> list of (clean, raw, atlas, relpath, w, h)
    buckets, used = {}, {}
    atlases = sorted(d for d in glob.glob(os.path.join(SLICED, "*")) if os.path.isdir(d))
    for adir in atlases:
        atlas = os.path.basename(adir)
        if atlas_filter and atlas != atlas_filter:
            continue
        for f in sorted(glob.glob(os.path.join(adir, "*.png"))):
            raw = os.path.basename(f)[:-4]
            cl = clean_name(raw)
            cat = category(cl)
            os.makedirs(os.path.join(CURATED, cat), exist_ok=True)
            key = (cat, cl)
            used[key] = used.get(key, 0) + 1
            fn = cl if used[key] == 1 else f"{cl}_{used[key]}"
            dst = os.path.join(CURATED, cat, f"{fn}.png")
            shutil.copy2(f, dst)
            try:
                w, h = Image.open(f).size
            except Exception:
                w = h = 0
            buckets.setdefault(cat, []).append((fn, raw, atlas, f"{cat}/{fn}.png", w, h))

    # standalone _dump textures: single icons kept whole; packed sheets (many alpha islands, e.g.
    # tx_uTex_fl_footer_btn) sliced into their individual icons like an atlas.
    def reg(cat, base):
        os.makedirs(os.path.join(CURATED, cat), exist_ok=True)
        key = (cat, base)
        used[key] = used.get(key, 0) + 1
        return base if used[key] == 1 else f"{base}_{used[key]}"

    stand = sheets = 0
    if not atlas_filter:
        for f in sorted(glob.glob(os.path.join(DUMP, "*.png"))):
            fname = os.path.basename(f)
            if not is_standalone_icon(fname):
                continue
            raw = re.sub(r"_\d+x\d+\.png$", "", fname)
            cl = clean_name(fname[:-4])
            cat = category(cl)
            img = Image.open(f).convert("RGBA")
            W, H = img.size
            boxes = cc_boxes(img) if max(W, H) >= 384 else []
            if len(boxes) >= 5:                              # packed sheet -> slice into its icons
                sheets += 1
                for j, (bx, by, bw, bh) in enumerate(sorted(boxes, key=lambda b: (b[1] // 24, b[0])), 1):
                    fn = reg(cat, f"{cl}_{j:03d}")
                    img.crop((bx, by, bx + bw, by + bh)).save(os.path.join(CURATED, cat, f"{fn}.png"))
                    buckets.setdefault(cat, []).append((fn, raw, "(sheet)", f"{cat}/{fn}.png", bw, bh))
                    stand += 1
            else:                                            # genuine single icon — keep whole
                fn = reg(cat, cl)
                img.save(os.path.join(CURATED, cat, f"{fn}.png"))
                buckets.setdefault(cat, []).append((fn, raw, "(standalone)", f"{cat}/{fn}.png", W, H))
                stand += 1
    print(f"(+{stand} standalone icons from _dump — {sheets} packed sheets sliced up)")

    _write_gallery(buckets)
    total = sum(len(v) for v in buckets.values())
    print(f"curated {total} sprites into {len(buckets)} categories -> {CURATED}")
    for cat in sorted(buckets):
        print(f"  {cat:12} {len(buckets[cat])}")
    print(f"\nOpen the gallery:  {os.path.join(CURATED, 'index.html')}")
    return total


def _write_gallery(buckets):
    css = """<style>
    body{margin:0;background:#1c1e22;color:#e6e6e6;font:14px/1.4 "Segoe UI",system-ui,sans-serif}
    header{position:sticky;top:0;background:#15171a;padding:12px 18px;border-bottom:1px solid #333;z-index:2}
    h1{margin:0;font-size:18px} .nav a{color:#8fd220;margin-right:12px;text-decoration:none;font-weight:600}
    .cat{padding:8px 18px 24px} h2{color:#8fd220;border-bottom:1px solid #333;padding-bottom:4px}
    .grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(120px,1fr));gap:10px}
    .cell{background:#26292e;border:1px solid #333;border-radius:8px;padding:6px;text-align:center}
    .cell .img{height:76px;display:flex;align-items:center;justify-content:center;
      background:repeating-conic-gradient(#2b2e33 0% 25%,#232529 0% 50%) 50%/16px 16px;border-radius:5px}
    .cell img{max-width:100%;max-height:72px;image-rendering:auto}
    .nm{font-weight:700;font-size:12px;margin-top:5px;word-break:break-all;color:#fff}
    .raw{font-size:9px;color:#8a8f98;word-break:break-all} .meta{font-size:9px;color:#6a6f78}
    </style>"""
    nav = " ".join(f'<a href="#{c}">{c} ({len(buckets[c])})</a>' for c in sorted(buckets))
    parts = [f"<!doctype html><meta charset=utf-8><title>Trackside icon library</title>{css}",
             f'<header><h1>Trackside icon library — {sum(len(v) for v in buckets.values())} named sprites</h1>'
             f'<div class="nav">{nav}</div></header>']
    for cat in sorted(buckets):
        items = sorted(buckets[cat], key=lambda x: x[0])
        cells = "".join(
            f'<div class="cell"><div class="img"><img src="{html.escape(rel)}" loading="lazy"></div>'
            f'<div class="nm">{html.escape(nm)}</div><div class="raw">{html.escape(raw)}</div>'
            f'<div class="meta">{w}×{h} · {html.escape(atlas)}</div></div>'
            for nm, raw, atlas, rel, w, h in items)
        parts.append(f'<section class="cat" id="{cat}"><h2>{cat} ({len(items)})</h2><div class="grid">{cells}</div></section>')
    with open(os.path.join(CURATED, "index.html"), "w", encoding="utf-8") as f:
        f.write("\n".join(parts))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--atlas", help="only curate this atlas (default: all in _sliced/)")
    a = ap.parse_args()
    if not os.path.isdir(SLICED):
        raise SystemExit(f"no _sliced/ folder — run `slice_atlas.py --manifest` first ({SLICED})")
    curate(a.atlas)


if __name__ == "__main__":
    main()
