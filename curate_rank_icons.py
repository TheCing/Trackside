#!/usr/bin/env python3
"""Cut the dumped Rank_tex atlas into per-rank emblem files for the optimizer.

Input:  <game>/trackside-icons/_dump/Rank_tex_2048x2048.rgba  (from "Dump icons" in-game;
        raw RGBA, VERTICALLY FLIPPED — D3D texture origin)
Output: native/trackside-icons/rank/<rank_id>.rgba (128x128) for rank ids 1..98
        + rank_contact_sheet.png in the scratch dir for eyeball verification.

The atlas is a 13x13 grid (157.54px cells) with an irregular packing — the cell->label
table below was read off the flipped atlas by eye (all 98 labels are legible). Rank ids
follow master.mdb single_mode_rank: 1..18 = G..SS+, then each U tier = base + 1..9.
"""

import os

from PIL import Image

GAME = r"G:\SteamLibrary\steamapps\common\UmamusumePrettyDerby"
SRC = os.path.join(GAME, "trackside-icons", "_dump", "Rank_tex_2048x2048.rgba")
ROOT = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(ROOT, "native", "trackside-icons", "rank")

# (row, col) -> label, rows top-to-bottom on the FLIPPED (upright) atlas.
GRID = [
    ["A", "UF8", "UC", "UA", "US8"],
    ["B+", "UF7", "UD9", "UB9", "US7"],
    ["B", "UF6", "UD8", "UB8", "US6"],
    ["C+", "UF5", "UD7", "UB7", "US5"],
    ["C", "UF4", "UD6", "UB6", "US4"],
    ["D+", "UF3", "UD5", "UB5", "US3"],
    ["D", "UF2", "UD4", "UB4", "US2"],
    ["E+", "UF1", "UD3", "UB3", "US1"],
    ["E", "UF", "UD2", "UB2", "US", "US9"],
    ["F+", "UG9", "UD1", "UB1", "UA1", "UA2", "UA3", "UA4", "UA5", "UA6", "UA7", "UA8", "UA9"],
    ["F", "UG8", "UD", "UC1", "UC2", "UC3", "UC4", "UC5", "UC6", "UC7", "UC8", "UC9", "UB"],
    ["G+", "UG7", "UF9", "UE", "UE1", "UE2", "UE3", "UE4", "UE5", "UE6", "UE7", "UE8", "UE9"],
    ["G", "A+", "S", "S+", "SS", "SS+", "UG", "UG1", "UG2", "UG3", "UG4", "UG5", "UG6"],
]

LETTERS = ["G", "G+", "F", "F+", "E", "E+", "D", "D+", "C", "C+", "B", "B+", "A", "A+", "S", "S+", "SS", "SS+"]
U_TIERS = ["UG", "UF", "UE", "UD", "UC", "UB", "UA", "US"]


def label_to_id(label):
    if label in LETTERS:
        return LETTERS.index(label) + 1
    tier = label.rstrip("0123456789")
    step = label[len(tier):]
    return 19 + U_TIERS.index(tier) * 10 + (int(step) if step else 0)


def main():
    os.makedirs(OUT, exist_ok=True)
    data = open(SRC, "rb").read()
    atlas = Image.frombytes("RGBA", (2048, 2048), data).transpose(Image.FLIP_TOP_BOTTOM)
    n = 13
    seen = {}
    for r, row in enumerate(GRID):
        for c, label in enumerate(row):
            rid = label_to_id(label)
            assert rid not in seen, f"dup id {rid} ({label} vs {seen.get(rid)})"
            seen[rid] = label
            x0, y0 = round(c * 2048 / n), round(r * 2048 / n)
            x1, y1 = round((c + 1) * 2048 / n), round((r + 1) * 2048 / n)
            cell = atlas.crop((x0, y0, x1, y1)).resize((128, 128))
            with open(os.path.join(OUT, f"{rid}.rgba"), "wb") as f:
                f.write(cell.tobytes())
    assert len(seen) == 98, f"expected 98 ranks, got {len(seen)}"
    print(f"wrote {len(seen)} rank emblems to {OUT}")

    # Contact sheet for eyeball verification (a spread of ids across the ladder).
    ids = [1, 13, 18, 19, 28, 29, 38, 49, 69, 89, 98]
    sheet = Image.new("RGBA", (128 * len(ids), 148), (20, 22, 24, 255))
    from PIL import ImageDraw

    d = ImageDraw.Draw(sheet)
    for i, rid in enumerate(ids):
        raw = open(os.path.join(OUT, f"{rid}.rgba"), "rb").read()
        sheet.paste(Image.frombytes("RGBA", (128, 128), raw), (i * 128, 0))
        d.text((i * 128 + 4, 130), f"id {rid} = {seen[rid]}", fill=(255, 255, 255, 255))
    dst = os.path.join(os.environ.get("TEMP", "."), "rank_contact_sheet.png")
    sheet.save(dst)
    print("contact sheet:", dst)


if __name__ == "__main__":
    main()
