# pingtray

Network latency at a glance: a tiny bar graph in the system tray, the full
history (minutes to months, zoomable and scrollable) one click away.

- Pings a few hosts you configure, every 2 s by default, with unprivileged ICMP.
- Tray icon: one bar per ping, the worst target wins. Height is log-scaled
  (1 ms to 1 s), colour is green / yellow / orange by threshold, a full red
  bar means a lost ping. The tooltip lists each target with its loss over 5 min.
- Click the icon (or *Show graph* in its menu) for the graph window: reply
  time per target with a min/max band, and packet loss underneath.
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

## Configuration

`~/.config/pingtray/config.toml`, created on first start. Targets are keyed by
name: rename one and its history starts afresh.

```toml
interval_secs = 2.0
timeout_ms = 1500
retention_days = 180
warn_ms = 60.0
bad_ms = 200.0
icon_samples = 16

[[target]]
name = "UCLouvain"
host = "130.104.1.1"

[[target]]
name = "Google DNS"
host = "8.8.8.8"
```

Host names are resolved every 5 minutes, IPv4 preferred.

## Commands

| Command | |
|---|---|
| `pingtray` / `pingtray run` | ping, record, show the tray icon |
| `pingtray collect` | same without icon (server, unsupported desktop) |
| `pingtray view` | graph window |
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
2. Location for each sample, from the phone's GPS (e.g. OwnTracks or
   GPSLogger posting to a small endpoint), stored in its own table and joined
   on time.
3. Map view of latency and loss along the way.
4. Transport mode guess (foot, bike, car, train) from speed, acceleration
   and proximity to railway lines.
