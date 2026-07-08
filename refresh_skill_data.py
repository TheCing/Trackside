#!/usr/bin/env python3
"""Regenerate the skill advisor's bundled data files from the game's master.mdb.

Run after every game update — the advisor's skill_data.json / skill_chains.json /
card_inherent.json are baked into the DLL via include_str!, and skills added by an
update are invisible to the optimizer until these are refreshed AND the DLL rebuilt.

  python refresh_skill_data.py            # rewrites data/*.json in place
  cd native && cargo build --release      # rebake

Sources (Global master.mdb is plain SQLite; only meta/assets are encrypted):
  skill_data + text_data(cat 47) + single_mode_skill_need_point + card_data +
  available_skill_set + single_mode_rank + card_data.chara_id
"""

import json
import os
import sqlite3

MDB = os.path.expandvars(r"%USERPROFILE%\AppData\LocalLow\Cygames\Umamusume\master\master.mdb")
DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")


def dump(name, obj):
    path = os.path.join(DATA, name)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(obj, f, ensure_ascii=False, indent=1)
    print(f"  {name}: {len(obj)} entries")


def main():
    con = sqlite3.connect(MDB)
    names = dict(con.execute("SELECT [index], text FROM text_data WHERE category=47"))
    need = dict(con.execute("SELECT id, need_skill_point FROM single_mode_skill_need_point"))

    # skill_data.json — every skill, schema-compatible with the original export.
    skills = {}
    for (sid, rarity, group_id, group_rate, grade, tag_id, icon_id, cat, dis, disp) in con.execute(
        "SELECT id, rarity, group_id, group_rate, grade_value, tag_id, icon_id, skill_category,"
        " disable_singlemode, disp_order FROM skill_data"
    ):
        skills[str(sid)] = {
            "name": names.get(sid, f"Skill {sid}"),
            "rarity": rarity,
            "group_id": group_id,
            "grade_value": grade,
            "need_skill_point": need.get(sid, 0),
            "disable_singlemode": dis,
            "tags": [int(t) for t in str(tag_id or "").split(",") if t.strip().isdigit()],
            "icon_id": icon_id,
            "skill_category": cat,
            "disp_order": disp,
        }
    dump("skill_data.json", skills)

    # skill_chains.json — group_id -> tiers sorted by group_rate (advisor chain walk).
    chains = {}
    for (sid, gid, gr, rar, grade) in con.execute(
        "SELECT id, group_id, group_rate, rarity, grade_value FROM skill_data WHERE group_id > 0"
    ):
        chains.setdefault(str(gid), []).append(
            {"id": sid, "group_rate": gr, "rarity": rar, "cost": need.get(sid, 0), "grade": grade}
        )
    for v in chains.values():
        v.sort(key=lambda m: (m["group_rate"], m["id"]))
    dump("skill_chains.json", chains)

    # card_inherent.json — card_id -> [{skill_id, need_rank}] via available_skill_set.
    cols = [r[1] for r in con.execute("PRAGMA table_info(card_data)")]
    if "available_skill_set_id" in cols:
        sets = {}
        for (set_id, skill_id, need_rank) in con.execute(
            "SELECT available_skill_set_id, skill_id, need_rank FROM available_skill_set"
        ):
            sets.setdefault(set_id, []).append({"skill_id": skill_id, "need_rank": need_rank})
        inherent = {}
        for (card, set_id) in con.execute("SELECT id, available_skill_set_id FROM card_data"):
            inherent[str(card)] = sets.get(set_id, [])
        dump("card_inherent.json", inherent)
    else:
        print("  card_inherent.json: SKIPPED (card_data schema changed — update this script)")

    # skill_roles.json — distance/style/surface applicability derived from the skills' own
    # trigger conditions (ground truth; replaces the stale name-keyed UmaLauncher CSV import).
    # Keyed by skill_id. Empty role = unconditional skill (filters always keep it).
    import re

    cond_re = re.compile(r"(?<![A-Za-z_])(distance_type|running_style|ground_type)==([0-9])")
    dist_map = {"1": "sprint", "2": "mile", "3": "medium", "4": "long"}
    style_map = {"1": "front", "2": "pace", "3": "late", "4": "end"}
    ground_map = {"1": "turf", "2": "dirt"}
    roles = {}
    for (sid, c1, c2) in con.execute("SELECT id, condition_1, condition_2 FROM skill_data"):
        parts = []
        for field, val in cond_re.findall((c1 or "") + "&" + (c2 or "")):
            tag = {"distance_type": dist_map, "running_style": style_map, "ground_type": ground_map}[field].get(val)
            if tag and tag not in parts:
                parts.append(tag)
        if parts:
            roles[str(sid)] = "/".join(parts)
    dump("skill_roles.json", roles)

    # card_chara.json + rank_ranges.json — cheap, refresh together.
    dump("card_chara.json", {str(r[0]): r[1] for r in con.execute("SELECT id, chara_id FROM card_data")})
    dump(
        "rank_ranges.json",
        [
            {"id": r[0], "min": r[1], "max": r[2]}
            for r in con.execute("SELECT id, min_value, max_value FROM single_mode_rank ORDER BY id")
        ],
    )
    print("done — rebuild the DLL to bake the new data in.")


if __name__ == "__main__":
    main()
