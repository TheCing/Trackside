#!/usr/bin/env python3
"""Fetch skill icons + chara portraits from GameTora into trackside-icons/.

This is the same source UmaLauncher uses for its helper-table art (see
helper_table.py: gametora.com/images/umamusume/...). Skill icons are keyed by the
master-data `icon_id`, portraits by `chara_id` — both already bundled in data/.

  python fetch_icons.py          # resumable; skips files that already exist

Outputs (raw RGBA, the overlay's native icon format):
  native/trackside-icons/skill/<icon_id>.rgba   64x64   (race HUD + optimizer cards)
  native/trackside-icons/uma/<chara_id>.rgba    64x64   (race HUD portraits)
  native/trackside-icons/chara/<chara_id>.rgba 128x128  (optimizer header portrait)
  native/trackside-icons/skill_icon_map.csv     skill_id,icon_id for every skill

Rank emblems are NOT on GameTora — those come from the in-game "Dump icons" ripper.
After fetching: copy native/trackside-icons/ next to the deployed DLL.
"""

import csv
import io
import json
import os
import time
import urllib.request

ROOT = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(ROOT, "native", "trackside-icons")
UA = {"User-Agent": "Trackside-icon-fetch/1.0 (one-time asset sync)"}

SKILL_URL = "https://gametora.com/images/umamusume/skill_icons/utx_ico_skill_{}.png"
CHARA_URL = "https://gametora.com/images/umamusume/characters/icons/chr_icon_{}.png"


def fetch_png(url):
    from PIL import Image

    req = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(req, timeout=20) as r:
        return Image.open(io.BytesIO(r.read())).convert("RGBA")


def save_rgba(img, px, path):
    if img.size != (px, px):
        img = img.resize((px, px))
    with open(path, "wb") as f:
        f.write(img.tobytes())


def main():
    skills = json.load(open(os.path.join(ROOT, "data", "skill_data.json"), encoding="utf-8"))
    charas = json.load(open(os.path.join(ROOT, "data", "card_chara.json"), encoding="utf-8"))
    for sub in ("skill", "uma", "chara"):
        os.makedirs(os.path.join(OUT, sub), exist_ok=True)

    # Full skill_id -> icon_id map (the overlay reads this CSV at boot).
    with open(os.path.join(OUT, "skill_icon_map.csv"), "w", newline="") as f:
        w = csv.writer(f)
        for sid, e in sorted(skills.items(), key=lambda kv: int(kv[0])):
            if e.get("icon_id"):
                w.writerow([sid, e["icon_id"]])

    icon_ids = sorted({e["icon_id"] for e in skills.values() if e.get("icon_id")})
    chara_ids = sorted(set(charas.values()))
    print(f"{len(icon_ids)} skill icons, {len(chara_ids)} charas")

    ok = miss = skip = 0
    for icon_id in icon_ids:
        dst = os.path.join(OUT, "skill", f"{icon_id}.rgba")
        if os.path.exists(dst):
            skip += 1
            continue
        try:
            save_rgba(fetch_png(SKILL_URL.format(icon_id)), 64, dst)
            ok += 1
        except Exception as e:
            print(f"  miss skill {icon_id}: {e}")
            miss += 1
        time.sleep(0.05)

    for cid in chara_ids:
        dst64 = os.path.join(OUT, "uma", f"{cid}.rgba")
        dst128 = os.path.join(OUT, "chara", f"{cid}.rgba")
        if os.path.exists(dst64) and os.path.exists(dst128):
            skip += 1
            continue
        try:
            img = fetch_png(CHARA_URL.format(cid))
            save_rgba(img, 64, dst64)
            save_rgba(img, 128, dst128)
            ok += 1
        except Exception as e:
            print(f"  miss chara {cid}: {e}")
            miss += 1
        time.sleep(0.05)

    print(f"done: {ok} fetched, {skip} already present, {miss} missing")
    print(f"now copy {OUT} next to the deployed DLL")


if __name__ == "__main__":
    main()
