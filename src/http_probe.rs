//! Tiny real HTTP requests, to compare with pings: a HEAD on a fresh
//! connection, timed phase by phase (DNS, TCP connect, TLS, first byte).

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url> {
        let s = s.trim();
        let (https, rest) = if let Some(r) = s.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("http://") {
            (false, r)
        } else {
            bail!("{s}: the URL must start with http:// or https://")
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        let default_port = if https { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            // "[::1]:8080" or "host:8080", but not a bare IPv6 address
            Some((h, p)) if !h.is_empty() && (h.ends_with(']') || !h.contains(':')) => {
                (h.to_string(), p.parse().with_context(|| format!("{s}: bad port"))?)
            }
            _ => (authority.to_string(), default_port),
        };
        if host.is_empty() {
            bail!("{s}: no host");
        }
        Ok(Url { https, host: host.trim_matches(['[', ']']).to_string(), port, path })
    }

    fn host_header(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == if self.https { 443 } else { 80 } { host } else { format!("{host}:{}", self.port) }
    }
}

/// Time spent in each phase of one request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Phases {
    pub dns: Duration,
    pub connect: Duration,
    /// Zero for plain HTTP.
    pub tls: Duration,
    /// From sending the request to the first byte of the answer.
    pub wait: Duration,
    pub status: u16,
}

impl Phases {
    pub fn total(&self) -> Duration {
        self.dns + self.connect + self.tls + self.wait
    }
}

fn tls_connector() -> tokio_rustls::TlsConnector {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(config)
    });
    tokio_rustls::TlsConnector::from(config.clone())
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, url: &Url) -> Result<(Duration, u16)> {
    let request = format!(
        "HEAD {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: pingtray/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path,
        url.host_header(),
        env!("CARGO_PKG_VERSION")
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let sent = Instant::now();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    let wait = sent.elapsed();
    if n == 0 {
        bail!("connection closed without an answer");
    }
    // "HTTP/1.1 204 No Content"
    let head = String::from_utf8_lossy(&buf[..n]);
    let status = head
        .strip_prefix("HTTP/")
        .and_then(|r| r.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .context("not an HTTP answer")?;
    Ok((wait, status))
}

/// One request. Any HTTP answer counts, whatever its status: the server
/// was reached. Errors (DNS, refused, TLS, timeout) count as lost.
pub async fn probe(url: &Url, timeout: Duration) -> Result<Phases> {
    tokio::time::timeout(timeout, async {
        let t = Instant::now();
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((url.host.as_str(), url.port)).await?.collect();
        let addr = addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().context("no address")?;
        let dns = t.elapsed();

        let t = Instant::now();
        let tcp = TcpStream::connect(addr).await?;
        tcp.set_nodelay(true)?;
        let connect = t.elapsed();

        let (tls, (wait, status)) = if url.https {
            let t = Instant::now();
            let name = rustls::pki_types::ServerName::try_from(url.host.clone())?;
            let stream = tls_connector().connect(name, tcp).await?;
            let tls = t.elapsed();
            (tls, exchange(stream, url).await?)
        } else {
            (Duration::ZERO, exchange(tcp, url).await?)
        };
        Ok(Phases { dns, connect, tls, wait, status })
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = Url::parse("https://www.google.com/generate_204").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.path.as_str()), (true, "www.google.com", 443, "/generate_204"));
        let u = Url::parse("http://192.168.1.1:8080").unwrap();
        assert_eq!((u.port, u.path.as_str()), (8080, "/"));
        assert_eq!(Url::parse("http://[::1]:81/x").unwrap().host_header(), "[::1]:81");
        assert!(Url::parse("ftp://x").is_err());
    }

    #[tokio::test]
    async fn plain_http_against_a_local_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await.unwrap();
        });
        let url = Url::parse(&format!("http://127.0.0.1:{port}/generate_204")).unwrap();
        let p = probe(&url, Duration::from_secs(2)).await.unwrap();
        assert_eq!(p.status, 204);
        assert_eq!(p.tls, Duration::ZERO);
    }
}
