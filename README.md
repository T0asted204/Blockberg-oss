# Blockberg

A Bloomberg-style trading terminal for the Hypixel SkyBlock Bazaar, plus the tools to run its in-game agents headless.

| Folder | What it is |
|---|---|
| `blockberg/` | Rust terminal + GUI (`blockberg`, `blockberg-gui`), the background server (`blockberg-gui --server`), the price recorder (`blockberg-rec`), and the in-game Minescript agent (`agent/`). |
| `bblauncher/` | BBlauncher: a small Rust Minecraft launcher (Microsoft login, Fabric install, mods) that can run the client **headless** so an agent runs without a window. |

> **No longer maintained.** Published as-is for reference.

> **Hypixel rules.** Automating gameplay on Hypixel is against its rules unless you have permission. This project was built with permission from Hypixel staff; you need your own. Use at your own risk.

## How it fits together

```
blockberg-gui  <-->  blockberg server (127.0.0.1:7780)  <-->  agent (Minescript, in Minecraft, :7777)
                                                               ^ client started by BBlauncher (optionally headless)
```

## Blockberg

```sh
cd blockberg
cargo build --release
./target/release/blockberg-gui mockup      # try the UI with fake data, no game needed
BLOCKBERG_MOCK=1 ./target/release/blockberg-gui
```

Agent: copy `agent/blockberg_agent.py` and `agent/bb_fast.pyj` into your instance's `minescript/` folder (Fabric + Minescript 5),
run `\blockberg_agent` in game (or set `autorun[*]=blockberg_agent` in `minescript/config.txt`).

Configuration lives in files and env vars, nothing is hardcoded to a machine:

- `BLOCKBERG_AGENT=host:port` agent address (default `127.0.0.1:7777`)
- `BLOCKBERG_PROFILE=<name>` + `~/.config/blockberg/agents.json` for multiple accounts (`tools/add-agent.sh`)
- `~/.config/blockberg/server_secret` created on first server start (mode 600)
- `~/.config/blockberg/hypixel_key`, `hypixel_uuid` optional, for bank balance via the Hypixel API
- `BLOCKBERG_DATA` data dir (default `~/.local/share/blockberg`)

Server as a user service: `tools/install-server.sh --now`. Price recorder + standby: see `tools/rec/README.md`.
More notes: `tools/DESKTOP.md`.

## BBlauncher

```sh
cd bblauncher
cargo build --release
./target/release/bblauncher login main                 # Microsoft device-code login
./target/release/bblauncher install main 26.1
./target/release/bblauncher mods add main fabric-api minescript
./headless/build-headless.sh                           # builds the headless LWJGL stub (see below)
./run-agent.sh main 7777                               # headless client -> hypixel.net -> SkyBlock
```

- Data: `~/.local/share/bblauncher` (override `BBLAUNCHER_HOME`). Account tokens are stored in `accounts/<label>.json` (mode 600). Keep them private.
- `BB_SERVER` changes the server for `run-agent.sh` / `xvfb-run.sh`.
- Keep an agent running: `./install-agent.sh`, then `systemctl --user enable --now bblauncher-agent` (or `bblauncher-agent@<name>-<port>`).
- `perf_options.py <instance>` sets the lowest-cost game options (run with the game closed).

### Headless mode

The headless stub comes from [HeadlessMC](https://github.com/headlesshq/headlessmc) (MIT), module `headlessmc-lwjgl`.
`headless/build-headless.sh` fetches a pinned revision, compiles it, relocates ASM (`headless/Relocate.java`) and writes
`lwjgl-agent.jar` (override the path with `BBLAUNCHER_LWJGL_AGENT`). The jar is not committed.

## Credits / third-party

- HeadlessMC (MIT) for the headless LWJGL stub.
- TradingView Lightweight Charts (Apache 2.0), bundled in `blockberg/assets/lwc.js`.
- DejaVu and Roboto Condensed fonts, licences in `blockberg/assets/fonts/`.
- Item data from the Hypixel API and the NotEnoughUpdates repo; item icons generated from Minecraft textures by `tools/build_icons.py`. Minecraft assets © Mojang. Not affiliated with Mojang, Microsoft or Hypixel.

## Licence

MIT, see `LICENSE`.
