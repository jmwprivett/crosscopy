//! Picking good addresses. Machines often have several (LAN, VPN such as
//! Tailscale, Hyper-V/WSL/Docker bridges), and only some reach the peer.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Orders a peer's advertised addresses best-first for dialing: addresses on
/// one of our own subnets, then private LAN ranges, then CGNAT/VPN, then rest.
pub fn rank_peer_addresses(addrs: impl IntoIterator<Item = IpAddr>, port: u16) -> Vec<SocketAddr> {
    rank_with_networks(addrs, port, &local_networks())
}

/// `local` holds `(network, mask)` pairs for this machine's interfaces.
fn rank_with_networks(
    addrs: impl IntoIterator<Item = IpAddr>,
    port: u16,
    local: &[(u32, u32)],
) -> Vec<SocketAddr> {
    let mut ranked: Vec<(u8, u8, Ipv4Addr)> = addrs
        .into_iter()
        .filter_map(usable_v4)
        .map(|ip| {
            let bits = u32::from(ip);
            let on_link = local.iter().any(|&(net, mask)| bits & mask == net);
            (u8::from(!on_link), class(ip), ip)
        })
        .collect();
    ranked.sort();
    ranked.dedup_by_key(|r| r.2);
    ranked.into_iter().map(|(_, _, ip)| SocketAddr::from((ip, port))).collect()
}

/// This machine's usable IPv4 addresses, most likely LAN address first.
pub fn local_addresses() -> Vec<Ipv4Addr> {
    let mut addrs: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|iface| usable_v4(iface.ip()))
        .collect();
    addrs.sort_by_key(|&ip| (class(ip), ip));
    addrs.dedup();
    addrs
}

fn usable_v4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// Lower is more likely to be a real LAN. 172.16/12 ranks below the others
/// because Hyper-V, WSL and Docker allocate their bridges from it.
fn class(ip: Ipv4Addr) -> u8 {
    match ip.octets() {
        [192, 168, ..] => 0,
        [10, ..] => 1,
        [172, b, ..] if (16..32).contains(&b) => 2,
        [100, b, ..] if (64..128).contains(&b) => 3,
        _ => 4,
    }
}

fn local_networks() -> Vec<(u32, u32)> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) => {
                let mask = u32::from(v4.netmask);
                Some((u32::from(v4.ip) & mask, mask))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(addrs: &[&str], local: &[(u32, u32)]) -> Vec<String> {
        let ips = addrs.iter().map(|s| s.parse::<IpAddr>().unwrap());
        rank_with_networks(ips, 1, local)
            .iter()
            .map(|a| a.ip().to_string())
            .collect()
    }

    fn net(cidr_ip: &str, prefix: u32) -> (u32, u32) {
        let mask = u32::MAX << (32 - prefix);
        (u32::from(cidr_ip.parse::<Ipv4Addr>().unwrap()) & mask, mask)
    }

    #[test]
    fn lan_ranks_above_vpn_and_virtual_bridges() {
        let addrs = ["100.101.1.2", "172.22.176.1", "192.168.1.20", "127.0.0.1", "169.254.3.3"];
        assert_eq!(ranked(&addrs, &[]), ["192.168.1.20", "172.22.176.1", "100.101.1.2"]);
    }

    #[test]
    fn same_subnet_wins() {
        let addrs = ["192.168.1.20", "10.0.0.7"];
        assert_eq!(ranked(&addrs, &[net("10.0.0.1", 24)]), ["10.0.0.7", "192.168.1.20"]);
    }
}
