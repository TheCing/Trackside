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

from PIL import Image

GAME = r"G:\SteamLibrary\steamapps\common\UmamusumePrettyDerby"
SLICED = os.path.join(GAME, "trackside-icons", "_sliced")
CURATED = os.path.join(GAME, "trackside-icons", "_curated")

# first token after the prefix strip -> folder/category
CATS = {"btn": "buttons", "frm": "frames", "txt": "text", "ico": "icons",
        "num": "numbers", "gau": "gauges", "bg": "backgrounds", "img": "images"}


def clean_name(name):
    """utx_btn_play_main_s_00 -> btn_play_main_s ;  ..._badge_performance00_0_C -> ..._badge_performance"""
    n = name.lower()
    for p in ("tx_utex_fl_", "tx_utex_", "utx_", "utex_"):
        if n.startswith(p):
            n = n[len(p):]
            break
    n = re.sub(r"(\d+)?_0_c$", "", n)   # _0_C / 00_0_C trailing variant marker
    n = re.sub(r"_00$", "", n)          # trailing _00
    return n or name.lower()


def category(clean):
    head = clean.split("_", 1)[0]
    if head in CATS:
        return CATS[head]
    if "_gau_" in f"_{clean}_":
        return "gauges"
    return "misc"


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
