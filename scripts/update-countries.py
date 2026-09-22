#!/usr/bin/env python3
"""Reproduce the bundled public-domain Natural Earth map without GIS dependencies.

Pinned upstream input, SHA256 checked; pass --source for an offline regeneration.
Country radius selection uses its documented cartographic reference points,
not these simplified borders as a promise of physical server distance.
"""
import argparse
import hashlib
import json
from pathlib import Path
import urllib.request

REVISION = "ca96624a56bd078437bca8184e78163e5039ad19"
SOURCE = f"https://raw.githubusercontent.com/nvkelso/natural-earth-vector/{REVISION}/geojson/ne_50m_admin_0_countries.geojson"
SHA256 = "3e458fc036ad0a66411f2c1e6cac49c5d7bfb81cb1123bc513b22511a2b7fdeb"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path)
    args = parser.parse_args()
    if args.source:
        raw = args.source.read_bytes()
    else:
        with urllib.request.urlopen(SOURCE, timeout=30) as response:
            raw = response.read(20_000_000)
    if hashlib.sha256(raw).hexdigest() != SHA256:
        raise SystemExit("Unexpected upstream data: SHA256 mismatch")
    countries = {}
    for feature in json.loads(raw)["features"]:
        props, geometry = feature["properties"], feature["geometry"]
        code = props["ISO_A2_EH"]
        if len(code) != 2 or not code.isascii() or not code.isalpha():
            continue  # Unassigned/disputed codes aren't valid Vast country selectors.
        polygons = geometry["coordinates"]
        if geometry["type"] == "Polygon":
            polygons = [polygons]
        area = sum(abs(sum(a[0] * b[1] - b[0] * a[1] for a, b in zip(polygon[0], polygon[0][1:]))) for polygon in polygons)
        entry = countries.setdefault(code, {"code": code, "name": "", "center": [], "polygons": [], "area": -1})
        if area > entry["area"]:
            entry.update(name=props["NAME_EN"], center=[props["LABEL_X"], props["LABEL_Y"]], area=area)
        entry["polygons"].extend([[[[round(x, 4), round(y, 4)] for x, y in ring] for ring in polygon] for polygon in polygons])
    data = []
    for code in sorted(countries):
        entry = countries[code]
        del entry["area"]
        data.append(entry)
    target = Path(__file__).resolve().parents[1] / "crates/router/static/countries.json"
    target.write_text(json.dumps(data, separators=(",", ":"), ensure_ascii=False) + "\n")
    print(f"{target}: {len(data)} country codes; upstream {REVISION} ({SHA256})")


if __name__ == "__main__":
    main()
