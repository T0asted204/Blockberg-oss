# Price recorder + standby

- `blockberg-rec` on the main machine records one candle per minute per listed item, 24/7, into `~/.local/share/blockberg/candles/`.
  The Blockberg chart reads those files, so the chart has all the history.
- `blockberg-rec --standby --peer MAIN_HOST:7790` on a second machine records too, but keeps only the last 10 minutes
  while the main recorder answers. If the main recorder is unreachable for ~30 s (or its price feed is silent for 4 min)
  it stops deleting and keeps recording. When the main machine is back it sends everything it kept (merge by minute, the fuller
  candle wins), then goes back to 10 minutes. While healthy it also sends its last 15 minutes every 5 minutes to fill small holes.
- Shared secret: `~/.config/blockberg/rec_secret` (created by the main recorder; copy to the standby machine, mode 600).
- One-off: `blockberg-rec --import-history ~/.local/share/blockberg/gui_history.json` turns the app's saved snapshots into candles.
- Install: copy the binary to `~/.local/bin/`, the unit to `~/.config/systemd/user/`, then
  `systemctl --user daemon-reload && systemctl --user enable --now <unit>`.

- The unit files contain `@MAIN_RECORDER_ADDR@`: replace it with the main machine's address, e.g. `sed -i 's/@MAIN_RECORDER_ADDR@/10.0.0.5:7790/' ~/.config/systemd/user/blockberg-rec*.service`.
