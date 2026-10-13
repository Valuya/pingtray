# pingtray

Network latency at a glance: a tiny bar graph in the system tray, the full
history (minutes to months, zoomable and scrollable) one click away.

- Pings a few hosts you configure, every 2 s by default, with unprivileged ICMP.
- Tray icon: the current median in ms (yellow / orange above the thresholds,
  red `--` when nothing comes back), then a SmokePing-style mini graph of the
  last 5 minutes, one column per 15 s, all targets pooled. The tooltip lists
  each target and gives the scale.
- Click the icon (or *Show graph* in its menu) for the graph window, drawn
  like SmokePing: one chart per target, grey smoke for the spread of the
  pings (min-max, 10-90 %, 25-75 %), the median as a line coloured by loss,
  red shading where everything was lost.
- Optional HTTP probes per destination: a tiny HEAD request on a fresh
  connection every 10 s, timed phase by phase (DNS, connect, TLS, answer),
  drawn dashed next to the pings, so you see what browsing actually feels
  like compared to the raw network.
- History lives in SQLite, so the graph covers everything since the first run
  (180 days by default).

## Install

```bash
cargo install --path .
pingtray autostart     # start with the desktop session
pingtray &             # or start it now
```

On GNOME the tray needs the AppIndicator extension (enabled by default on
Ubuntu). KDE, XFCE and most wlroots bars support it natively.

Linux needs unprivileged ICMP, which most distributions allow:
`sysctl net.ipv4.ping_group_range` must include your group.

## Settings

*Settings…* in the tray menu (or ⚙ in the graph window) opens the settings
window: destinations, ping timing, tray graph, phone positions. Saving
applies the changes at once (pingtray restarts itself).

They are stored in `~/.config/pingtray/config.toml`. History is kept per
destination name: renaming one starts a new history.

## Notes

*Add note…* in the tray menu (or *Note…* in the graph window, or
`pingtray note crowded` in a terminal) records what the graph cannot know:
crowded train, quiet carriage, Wi-Fi on board. Notes show as markers on
the charts, and the tooltip gives the latest one before the moment under
the mouse.

## Battery charging (Linux)

Settings → Power. The battery icon's menu picks the mode:

- **Automatic**: pingtray decides. Maximize Charge when plugged in while
  travelling (moving according to the phone's positions, or connected to a
  network you list, such as the phone's hotspot), Preserve Battery Health
  otherwise. An "A" on the icon shows it.
- **Maximize charge** / **Preserve battery health**: manual, kept until you
  pick Automatic again. Changing the mode in GNOME's settings also switches
  to manual.

It uses UPower's charge threshold, the same switch as GNOME's settings.

## Phone positions

**Recommended: the phone's GPS over ADB.** Connect the phone with a USB
cable, or pair it once for wireless debugging (`adb pair`). pingtray then
reads its latest fix every 15 s (`dumpsys location`) and keeps `adb reverse`
up, so that OwnTracks on the phone can post to `http://127.0.0.1:18765/pub`,
an address that never changes. Only the local link is used: positions keep
coming where there is no mobile coverage, which is what pingtray is looking
for. Keep an app asking for location (OwnTracks in Move mode) so that the
phone's GPS stays on.

The computer's own location service (GeoClue: Wi-Fi positioning, or a GPS
stream shared on the local network) is used too; IP-based guesses (often
tens of km off) are ignored.

Other ways:

Turn on *Receive positions from my phone* in the settings; the window then
shows the URL to enter in the phone app:

- **OwnTracks** (Android, iOS): mode HTTP, the URL shown, any user name,
  the token as password.
- **GPSLogger** (Android): *Log to custom URL* with the URL shown.

The phone has to reach this computer (same Wi-Fi, or the laptop tethered on
the phone). Otherwise let the app write GPX or CSV files to a folder synced
to the laptop and pick that folder in *Import position files from a folder*;
`pingtray import FILE…` loads files by hand.

Each ping is then placed between the positions around it. The map shows
the uncertainty: accuracy discs around each fix, fainter lines where the
fix was vague, and a circle sized to the uncertainty at the moment under
the mouse (accuracy plus the leeway of interpolating between fixes).

The transport mode is guessed from speed patterns, then from the distance
to OpenStreetMap railways (fetched from Overpass, cached per tile) to tell
trains and trams from cars. `pingtray trips` lists the journeys.

## Commands

| Command | |
|---|---|
| `pingtray` / `pingtray run` | ping, record, show the tray icon |
| `pingtray collect` | same without icon (server, unsupported desktop) |
| `pingtray view` | graph window |
| `pingtray settings` | settings window |
| `pingtray import FILE…` | load positions (GPX, CSV, OwnTracks .rec, GeoJSON) |
| `pingtray trips [--hours N]` | journeys with their transport mode |
| `pingtray autostart [--remove]` | XDG autostart entry |

## Graph window

Wheel zooms the time axis around the pointer, dragging scrolls, horizontal
scroll pans, double-click goes back to live. The range buttons jump to the
last 5 minutes to 30 days, *All* to the whole history. Points are aggregated
into buckets (1 s up to 1 day) so that any range stays around 1200 points.

## Layout

| File | Role |
|---|---|
| `src/pinger.rs` | one tokio task per target (surge-ping) |
| `src/collector.rs` | stores samples, keeps the recent window for the icon |
| `src/store.rs` | SQLite schema, bucketed range queries |
| `src/icon.rs` | renders the tray bars to RGBA |
| `src/tray.rs` | tray backend (StatusNotifierItem via `ksni` on Linux) |
| `src/viewer.rs` | egui / egui_plot window, queries in a worker thread |

Porting: everything but `src/tray.rs` is portable. A Windows/macOS backend
would implement `TrayHandle` with the `tray-icon` crate; until then
`pingtray collect` + `pingtray view` work there.

Data: `~/.local/share/pingtray/pingtray.sqlite` (WAL, one writer, readers
welcome). Table `sample(target_id, ts_ms, rtt_us)`, `rtt_us` NULL for a lost
ping.

## Roadmap

1. ~~Tray graph and history window.~~
2. ~~Positions from the phone, map, transport mode.~~
3. Windows/macOS tray backend (`tray-icon` crate).
