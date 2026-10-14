//! Keeps this computer reachable as `<hostname>.local`, the address the
//! phone posts positions to.
//!
//! After a network change (resume, Wi-Fi reconnect), avahi sometimes takes
//! its own earlier announcement, still cached on the network, for another
//! machine and renames itself `<hostname>-2.local`: the phone then finds
//! nothing. pingtray puts the name back. Avahi only lets root and the
//! `netdev` group do that.

use std::time::Duration;

use anyhow::Result;
use zbus::blocking::{Connection, Proxy};

const AVAHI: &str = "org.freedesktop.Avahi";

fn server(conn: &Connection) -> Result<Proxy<'_>> {
    Ok(Proxy::new(conn, AVAHI, "/", "org.freedesktop.Avahi.Server")?)
}

pub fn hostname() -> Option<String> {
    let h = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let h = h.trim().to_string();
    (!h.is_empty()).then_some(h)
}

/// The name avahi currently announces, e.g. "ymajoros.local".
pub fn announced() -> Option<String> {
    let conn = Connection::system().ok()?;
    server(&conn).ok()?.call("GetHostNameFqdn", &()).ok()
}

fn restore(host: &str) -> Result<()> {
    let conn = Connection::system()?;
    server(&conn)?.call_method("SetHostName", &(host,))?;
    Ok(())
}

pub fn spawn_watchdog() {
    std::thread::spawn(|| {
        let Some(host) = hostname() else { return };
        let wanted = format!("{host}.local");
        let mut complained = false;
        loop {
            if let Some(current) = announced()
                && current != wanted
            {
                match restore(&host) {
                    Ok(()) => {
                        tracing::info!("mDNS name was {current}: restored {wanted}");
                        complained = false;
                    }
                    Err(e) if !complained => {
                        tracing::warn!(
                            "mDNS name is {current}, not {wanted}, and could not be restored ({e}); \
                             adding the user to the netdev group allows it"
                        );
                        complained = true;
                    }
                    Err(_) => {}
                }
            }
            std::thread::sleep(Duration::from_secs(30));
        }
    });
}
