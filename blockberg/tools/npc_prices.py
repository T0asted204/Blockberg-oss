#!/usr/bin/env python3
"""Refresh assets/npc_sell.json (what an NPC pays you: Hypixel items API) and assets/npc_buy.json
(what an NPC shop charges you: coin-only npc_shop recipes in the NotEnoughUpdates item repo; cheapest shop wins).
Run from the blockberg directory:  python3 tools/npc_prices.py"""
import io, json, tarfile, urllib.request

def get(url): return urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "blockberg"}), timeout=240).read()

items = json.loads(get("https://api.hypixel.net/v2/resources/skyblock/items"))["items"]
sell = {i["id"]: i["npc_sell_price"] for i in items if isinstance(i.get("npc_sell_price"), (int, float))}
json.dump(sell, open("assets/npc_sell.json", "w"), separators=(",", ":"))
# item facts for the info window: id -> [rarity tier, category, unstackable]
meta = {i["id"]: [i.get("tier") or "", i.get("category") or "", 1 if i.get("unstackable") else 0] for i in items}
json.dump(meta, open("assets/item_meta.json", "w"), separators=(",", ":"))

buy = {}
tf = tarfile.open(fileobj=io.BytesIO(get("https://codeload.github.com/NotEnoughUpdates/NotEnoughUpdates-REPO/tar.gz/refs/heads/master")), mode="r:gz")
for m in tf:
    if not (m.isfile() and "/items/" in m.name and m.name.endswith("_NPC.json")): continue
    try: d = json.load(tf.extractfile(m))
    except Exception: continue
    for r in d.get("recipes") or []:
        if r.get("type") != "npc_shop": continue
        cost = r.get("cost") or []
        if len(cost) != 1 or not cost[0].startswith("SKYBLOCK_COIN:"): continue
        res = (r.get("result") or "").split(":")
        if not res[0]: continue
        qty = float(res[1]) if len(res) > 1 else 1.0
        unit = float(cost[0].split(":")[1]) / (qty or 1.0)
        if res[0] not in buy or unit < buy[res[0]]: buy[res[0]] = round(unit, 4)
json.dump(buy, open("assets/npc_buy.json", "w"), separators=(",", ":"))
print(f"{len(sell)} NPC sell prices, {len(buy)} NPC buy prices")
