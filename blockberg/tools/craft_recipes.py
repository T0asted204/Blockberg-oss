#!/usr/bin/env python3
"""Builds assets/craft_recipes.json: id -> [output count, [[ingredient, count], ...]] for crafting-table recipes of every bazaar item
(plus the non-bazaar intermediates they need), from the NotEnoughUpdates item repo. Run from the blockberg directory:  python3 tools/craft_recipes.py"""
import io, json, re, tarfile, urllib.request
def get(url): return urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "blockberg"}), timeout=240).read()
bz = set(json.loads(get("https://api.hypixel.net/v2/skyblock/bazaar"))["products"])
rec = {}
tf = tarfile.open(fileobj=io.BytesIO(get("https://codeload.github.com/NotEnoughUpdates/NotEnoughUpdates-REPO/tar.gz/refs/heads/master")), mode="r:gz")
for m in tf:
    if not (m.isfile() and "/items/" in m.name and m.name.endswith(".json")): continue
    try: d = json.load(tf.extractfile(m))
    except Exception: continue
    iid = d.get("internalname") or m.name.rsplit("/", 1)[-1][:-5]
    cands = []
    if isinstance(d.get("recipe"), dict): cands.append(d["recipe"])
    for r in d.get("recipes") or []:
        if isinstance(r, dict) and r.get("type") in (None, "crafting") and any(k in r for k in ("A1", "B2")): cands.append(r)
    for r in cands:
        ing = {}
        for k in ("A1","A2","A3","B1","B2","B3","C1","C2","C3"):
            v = r.get(k)
            if not v: continue
            name, _, n = v.partition(":")
            if name: ing[name] = ing.get(name, 0) + (float(n) if n else 1.0)
        if ing: rec[iid] = [float(r.get("count", 1) or 1), [[a, n] for a, n in ing.items()]]; break
# keep bazaar items and whatever their recipes (recursively) need
keep, todo = set(), [i for i in bz if i in rec]
while todo:
    i = todo.pop()
    if i in keep: continue
    keep.add(i)
    for a, _ in rec[i][1]:
        if a not in bz and a in rec: todo.append(a)
out = {i: rec[i] for i in sorted(keep)}
json.dump(out, open("assets/craft_recipes.json", "w"), separators=(",", ":"))
print(len(rec), "recipes found;", len(out), "kept;", sum(1 for i in bz if i in out), "of", len(bz), "bazaar items have a recipe")
