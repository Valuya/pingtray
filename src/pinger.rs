//! ICMP echo loop, one task per target. Uses unprivileged ICMP datagram sockets
//! where the OS allows them (Linux: net.ipv4.ping_group_range), raw sockets otherwise.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use surge_ping::{Client, ICMP, PingIdentifier, PingSequence, Pinger};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::config::{Config, Target};

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Index of the target in `Config::targets`.
    pub target: usize,
    pub ts_ms: i64,
    pub rtt: Option<Duration>,
}

const RESOLVE_EVERY: Duration = Duration::from_secs(300);
const PAYLOAD: [u8; 32] = *b"pingtray pingtray pingtray ping!";

struct Clients {
    v4: Option<Client>,
    v6: Option<Client>,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Starts one ping task per configured target; samples arrive on `tx`.
pub fn spawn_all(cfg: Arc<Config>, tx: mpsc::Sender<Sample>) -> anyhow::Result<()> {
    let client = |kind| Client::new(&surge_ping::Config::builder().kind(kind).build());
    let v4 = client(ICMP::V4).map_err(|e| tracing::warn!("ICMPv4 socket: {e}")).ok();
    let v6 = client(ICMP::V6).map_err(|e| tracing::warn!("ICMPv6 socket: {e}")).ok();
    if v4.is_none() && v6.is_none() {
        anyhow::bail!(
            "cannot open an ICMP socket; on Linux check `sysctl net.ipv4.ping_group_range`"
        );
    }
    let clients = Arc::new(Clients { v4, v6 });
    for (idx, target) in cfg.targets.iter().cloned().enumerate() {
        tokio::spawn(target_loop(idx, target, cfg.clone(), clients.clone(), tx.clone()));
    }
    Ok(())
}

async fn resolve(host: &str) -> Option<IpAddr> {
    if let Ok(ip) = host.parse() {
        return Some(ip);
    }
    let addrs: Vec<_> = tokio::net::lookup_host((host, 0)).await.ok()?.collect();
    // Prefer IPv4: it is what most "is the network up" checks mean.
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).map(|a| a.ip())
}

async fn target_loop(
    idx: usize,
    target: Target,
    cfg: Arc<Config>,
    clients: Arc<Clients>,
    tx: mpsc::Sender<Sample>,
) {
    let mut ticker = tokio::time::interval(cfg.interval());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let ident = PingIdentifier((std::process::id() as u16).wrapping_mul(31).wrapping_add(idx as u16));
    let mut pinger: Option<(IpAddr, Pinger)> = None;
    let mut resolved_at: Option<Instant> = None;
    let mut seq: u16 = 0;

    loop {
        ticker.tick().await;
        let ts_ms = now_ms();

        if pinger.is_none() || resolved_at.is_none_or(|t| t.elapsed() > RESOLVE_EVERY) {
            resolved_at = Some(Instant::now());
            match resolve(&target.host).await {
                Some(ip) if pinger.as_ref().map(|(cur, _)| *cur) != Some(ip) => {
                    let client = if ip.is_ipv4() { &clients.v4 } else { &clients.v6 };
                    pinger = match client {
                        Some(c) => {
                            let mut p = c.pinger(ip, ident).await;
                            p.timeout(cfg.timeout());
                            tracing::info!("{} -> {ip}", target.name);
                            Some((ip, p))
                        }
                        None => None,
                    };
                }
                Some(_) => {}
                None => {
                    tracing::debug!("{}: cannot resolve {}", target.name, target.host);
                    pinger = None;
                    resolved_at = None;
                }
            }
        }

        let rtt = match pinger.as_mut() {
            Some((_, p)) => p.ping(PingSequence(seq), &PAYLOAD).await.ok().map(|(_, d)| d),
            None => None,
        };
        seq = seq.wrapping_add(1);
        if tx.send(Sample { target: idx, ts_ms, rtt }).await.is_err() {
            return;
        }
    }
}
