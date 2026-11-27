#!/usr/bin/env python3
"""Why did an iceberg pause (or stop)? Reads the journal and prints the state changes of every iceberg job with the reason,
together with the agent up/down events and dropped actions around them.
   tools/why-paused.py            last 24 hours
   tools/why-paused.py 72         last 72 hours
   tools/why-paused.py all        everything in the journal (and journal.old)"""
import json, os, sys, datetime
base = os.path.expanduser("~/.local/share/blockberg/journal.jsonl")
arg = sys.argv[1] if len(sys.argv) > 1 else "24"
since = 0 if arg == "all" else datetime.datetime.now().timestamp() - float(arg) * 3600
rows = []
for p in (base + ".old".replace(".jsonl", ""), base):
    q = base.replace(".jsonl", ".old") if p.endswith(".old") else base
    try:
        for l in open(q):
            try: r = json.loads(l)
            except Exception: continue
            if r.get("t", 0) >= since: rows.append(r)
    except FileNotFoundError: pass
rows.sort(key=lambda r: r["t"])
def ts(r): return datetime.datetime.fromtimestamp(r["t"]).strftime("%m-%d %H:%M:%S")
shown = 0
for r in rows:
    ev = r.get("ev")
    if ev == "ice":
        mark = "  <== PAUSED" if r["state"] == "Paused" else ""
        print(f'{ts(r)}  iceberg #{r["id"]} {r["product"]}: {r["state"]}  {r["filled"]}/{r["total"]}  agent={"up" if r.get("agent_up") else "DOWN"}  inflight={r.get("inflight")}{mark}')
        if r["state"] in ("Paused", "Stopped") and r.get("note"): print(f'            reason: {r["note"]}')
        shown += 1
    elif ev == "agent":
        extra = f'  (was doing: {", ".join(r["pending"])})' if r.get("pending") and not r["up"] else ""
        print(f'{ts(r)}  agent {"CONNECTED" if r["up"] else "DISCONNECTED"}{extra}'); shown += 1
    elif ev == "dropped":
        print(f'{ts(r)}  {r["count"]} action(s) dropped: {r["why"]}'); shown += 1
    elif ev in ("boot", "restart"):
        print(f'{ts(r)}  server {ev}'); shown += 1
if not shown: print("nothing recorded in that window (state changes are logged from 2026-11-12 on)")
