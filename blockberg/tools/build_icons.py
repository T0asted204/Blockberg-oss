#!/usr/bin/env python3
"""Fill in item icons the sheet is missing (or only has a placeholder for) from Hypixel's server resource pack
(the one the game downloads into instances/*/game/downloads) and the vanilla client jar.
usage: build_icons.py PACK.zip CLIENT.jar   (rewrites assets/icons.png + assets/icons.json; rebuild the app after)"""
import io, json, sys, zipfile, urllib.request, pathlib
from PIL import Image

ROOT = pathlib.Path(__file__).resolve().parent.parent
S = 16
pack, jar = zipfile.ZipFile(sys.argv[1]), zipfile.ZipFile(sys.argv[2])


def read(ns, kind, path):
    name = f"assets/{ns}/{kind}/{path}"
    for z in (pack, jar):
        try:
            return z.read(name)
        except KeyError:
            pass
    return None


def split(ref, default="minecraft"):
    ns, _, p = ref.partition(":") if ":" in ref else (default, "", ref)
    return ns, p


def texture_of_model(ref, depth=0):
    """Follow a model (and its parents) to its first layer texture."""
    if depth > 8:
        return None
    ns, p = split(ref)
    raw = read(ns, "models", p + ".json")
    if raw is None:
        return None
    m = json.loads(raw)
    tex = m.get("textures", {})
    for key in ("layer0", "all", "top", "side", "particle", "cross", "plant", "texture"):
        t = tex.get(key)
        if t and not t.startswith("#"):
            return t if ":" in t else f"{ns}:{t}" if ns != "minecraft" else t
    if m.get("parent"):
        return texture_of_model(m["parent"], depth + 1)
    return None


def find_model(node):
    """The first model reference in an items/*.json definition tree."""
    if isinstance(node, dict):
        if node.get("type") in ("minecraft:model", "model") and "model" in node:
            return node["model"]
        for v in node.values():
            r = find_model(v)
            if r:
                return r
    elif isinstance(node, list):
        for v in node:
            r = find_model(v)
            if r:
                return r
    return None


def icon(item_model):
    ns, p = split(item_model)
    model = None
    raw = read(ns, "items", p + ".json")
    if raw:
        model = find_model(json.loads(raw))
    tex = texture_of_model(model or f"{ns}:item/{p.split('item/')[-1]}" if ns != "minecraft" else (model or f"minecraft:item/{p}"))
    cands = [tex] if tex else []
    cands += [f"{ns}:item/{p.split('item/')[-1]}", f"{ns}:{p}", f"{ns}:block/{p.split('/')[-1]}"]
    for c in cands:
        tns, tp = split(c)
        data = read(tns, "textures", tp + ".png")
        if data:
            im = Image.open(io.BytesIO(data)).convert("RGBA")
            w = im.width
            im = im.crop((0, 0, w, w))  # animated strips: first frame
            return im.resize((S, S), Image.NEAREST)
    return None


def get(url):
    return json.load(urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "blockberg-icons"})))


meta = json.loads((ROOT / "assets/icons.json").read_text())
sheet = Image.open(ROOT / "assets/icons.png").convert("RGBA")
cols, index = meta["cols"], meta["index"]
tile = lambda i: sheet.crop(((i % cols) * S, (i // cols) * S, (i % cols + 1) * S, (i // cols + 1) * S)).tobytes()

items = {i["id"]: i for i in get("https://api.hypixel.net/v2/resources/skyblock/items")["items"]}
bazaar = get("https://api.hypixel.net/v2/skyblock/bazaar")["products"]

# a tile shared by many unrelated items is a placeholder (except enchanted books and shards, which really do share one look)
from collections import Counter
uses = Counter(tile(i) for k, i in index.items() if not k.startswith(("ENCHANTMENT_", "SHARD_")))
placeholder = {t for t, n in uses.items() if n >= 20}
need = [k for k in bazaar if k not in index or (not k.startswith(("ENCHANTMENT_", "SHARD_")) and tile(index[k]) in placeholder)]

made, failed = 0, []
nxt = max(index.values()) + 1
for k in sorted(need):
    it = items.get(k)
    im = icon(it["item_model"]) if it and it.get("item_model") else None
    if im is None and it:
        im = icon("minecraft:" + it.get("material", "").lower())
    if im is None:
        failed.append(k)
        continue
    i = nxt
    nxt += 1
    rows = i // cols + 1
    if rows * S > sheet.height:
        bigger = Image.new("RGBA", (sheet.width, rows * S))
        bigger.paste(sheet, (0, 0))
        sheet = bigger
    sheet.paste(im, ((i % cols) * S, (i // cols) * S))
    index[k] = i
    made += 1

sheet.save(ROOT / "assets/icons.png", optimize=True)
meta["index"] = index
(ROOT / "assets/icons.json").write_text(json.dumps(meta))
print(f"needed {len(need)}, made {made}, still without: {len(failed)} {failed[:30]}")
