# Interface notes

The GUI is `src/bin/gui.rs`. It has the Exchange selector (Bazaar, NPC Trades, and an
Auction placeholder), NPC quantity/fill/all modes, inventory-aware quantities, quantity
presets, price-change quote figures, chart pan/zoom and axis drags.
NPC prices are bundled from `assets/npc_sell.json` / `assets/npc_buy.json` (refresh with `tools/npc_prices.py`).

The application launcher invokes `tools/launch-gui.sh`, which builds the current
release before starting it. Close and reopen the app after source edits.

Build and test:

```sh
cargo test --release --bin blockberg-gui
cargo build --release --bin blockberg-gui
```

Chart: drag to pan; drag the price/time axes to scale; scroll over the chart or
use −/+ to zoom; use ⟲ to reset. Changing the instrument or range resets the view.
The ticket scrolls vertically when its limit controls and checks exceed the window.

## Iceberg, queue, inventory, saved watchlist

Implemented in `src/iceberg.rs` (included by `gui.rs`); the red banner has new **Queue** and **Inventory** tabs
(commands `QUEUE`/`ICE`, `INV`, `MAIN`; Esc or the × returns to trading). These screens replace the watchlist,
chart, depth and ticket; the Blotter/Positions/Escrow/Logs/Chat panel at the bottom stays.

- **Iceberg (buy):** item, total (`2m`, `500k`), price min/max, offset over the top bid (0.1), requote minutes (5),
  max per order (71,680). Places one buy order at top bid + offset (never above max). Filled -> next slice at once.
  Not completely filled after the requote time -> read the order menu, cancel the rest (the filled part is kept),
  place a fresh order at the new top bid. Respects the purse (there is no daily spend cap any more). Jobs are saved in
  `~/.local/share/blockberg/iceberg_jobs.json` and come back **paused** after a restart.
- **Queue screen:** iceberg jobs with next step, countdown, step durations and an ETA from the observed fill rate,
  plus in-flight orders, claims, workflow steps and the background polls.
- **Inventory screen:** needs the agent command `invall` (agent version 2026-11-10.4, copy
  `agent/blockberg_agent.py` into the game's `minescript/` folder and Settings -> Restart agent).
- **Watchlist:** saved to `~/.local/share/blockberg/watchlist.json` on every change.
- Debug aids: `BLOCKBERG_SCREEN=queue|inventory`, `BLOCKBERG_DEMO_ICE=<ITEM>`, `BLOCKBERG_INV_OPEN=1`.

## Fast in-game helper (bb_fast.pyj)
`agent/bb_fast.pyj` runs inside Minecraft (Minescript's Pyjinn interpreter) and does menu clicks, closing menus and sign text in about a
millisecond instead of ~0.1-0.3 s through the Java bridge. It listens on 127.0.0.1:7779 and the agent starts it by itself.
Install: copy BOTH `blockberg_agent.py` and `bb_fast.pyj` into the game's `minescript/` folder, then Settings -> Restart agent.
If the helper is missing the agent silently uses the slower route. To restart the helper: send `exit` to it, then restart the agent.
Timing rules learned the hard way: Hypixel ignores a click on a menu younger than ~0.15 s (agent waits 0.22 s), a sign confirmed in the same
tick as its text (agent waits 0.06 s between), and a menu opened < ~0.25 s after the last one; it also puts order placing on a cooldown
("Placing orders is on cooldown for up to 1 minute") after about a dozen orders in a few minutes.

## Server and app (blockberg-gui --server)
Three modes in one binary:
* `blockberg-gui --server` - no window. Owns the agent connection, iceberg jobs, claims, the action queue and a journal
  (`~/.local/share/blockberg/journal.jsonl`). Listens on 127.0.0.1:7780; every connection must send the shared secret from
  `~/.config/blockberg/server_secret` (created on the first start, mode 600).
* the normal app - when a server is running it automatically becomes its window ("SERVER ✔  AGENT ✔" in the status bar). It sends
  commands to the server and shows what the server knows. Close, rebuild or crash it: nothing changes on the server.
* the normal app with no server running behaves as before (standalone). Two engines are never allowed at once (port 7781 lock).
Install: `tools/install-server.sh --now` (a systemd user service, restarts itself if it ever dies).
Restart: Settings -> "Restart server (after current work)". It is queued with high priority: it waits for what is running, starts nothing new
(held-back claims and the next orders wait too), saves, restarts, and the iceberg jobs are taken over from their saved state and the real order menu.
Unfinished actions from before a stop are listed in the server log on the next start (from the journal).
Not on the server (stays in the app): the watchlist, layout, price history, chart.
Testing: `BLOCKBERG_MOCK=1` runs the app against fake data and never touches the real agent; `BLOCKBERG_CLIENT=1` forces client mode.
