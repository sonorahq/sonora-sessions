//! Which address a request really came from. The server stands behind a reverse proxy, which
//! tells it the address it was reached from in `X-Forwarded-For`. When that address belongs to
//! Cloudflare, Cloudflare's own `CF-Connecting-IP` names the client. Anyone else could write that
//! header themselves, so it is only believed from a trusted proxy.

use std::net::IpAddr;

use anyhow::{Context as _, Result};
use axum::http::HeaderMap;

/// Cloudflare's ranges, as https://www.cloudflare.com/ips lists them. `TRUSTED_PROXIES` replaces
/// them for a server behind another proxy.
const CLOUDFLARE: [&str; 22] = [
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

/// The proxies whose word on the client's address is taken.
pub struct Trusted(Vec<(IpAddr, u8)>);

impl Trusted {
    /// Cloudflare's ranges, or the comma-separated ranges `TRUSTED_PROXIES` names instead. An
    /// empty `TRUSTED_PROXIES` trusts no proxy.
    pub fn from_env() -> Result<Self> {
        let ranges = match std::env::var("TRUSTED_PROXIES") {
            Ok(listed) => listed
                .split(',')
                .map(str::trim)
                .filter(|range| !range.is_empty())
                .map(range)
                .collect::<Result<_>>()?,
            Err(_) => CLOUDFLARE.into_iter().map(range).collect::<Result<_>>()?,
        };
        Ok(Self(ranges))
    }

    fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|&(base, bits)| within(ip, base, bits))
    }

    /// The client's address. The proxy in front of the server says in `X-Forwarded-For` where it
    /// was reached from, or `peer` is it when no proxy is in front. Only when that is a trusted
    /// proxy is its `CF-Connecting-IP` taken over it.
    pub fn client(&self, headers: &HeaderMap, peer: IpAddr) -> IpAddr {
        let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        let reached = header("x-forwarded-for")
            .and_then(|forwarded| forwarded.rsplit(',').next()?.trim().parse().ok())
            .unwrap_or(peer)
            .to_canonical();
        if !self.contains(reached) {
            return reached;
        }
        header("cf-connecting-ip")
            .and_then(|ip| ip.trim().parse().ok())
            .unwrap_or(reached)
    }
}

/// A range written as `address/bits`.
fn range(written: &str) -> Result<(IpAddr, u8)> {
    let (base, bits) = written
        .split_once('/')
        .with_context(|| format!("{written} is not a range"))?;
    let base: IpAddr = base
        .parse()
        .with_context(|| format!("cannot read {written}"))?;
    let bits: u8 = bits
        .parse()
        .with_context(|| format!("cannot read {written}"))?;
    let most = match base {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    anyhow::ensure!(bits <= most, "{written} has too many bits");
    Ok((base, bits))
}

/// Whether `ip` lies in the range `base/bits`. Addresses of the other family never do.
fn within(ip: IpAddr, base: IpAddr, bits: u8) -> bool {
    match (ip, base) {
        (IpAddr::V4(ip), IpAddr::V4(base)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(ip) & mask == u32::from(base) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(base)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(ip) & mask == u128::from(base) & mask
        }
        _ => false,
    }
}
