//! mDNS / DNS-SD for `_airdrop._tcp` on one interface (`awdl0`).
//!
//! Not a general responder: one socket bound to the interface, our four
//! records, and a browser for phones. Two AWDL habits shape it: the phone
//! never polls for receivers, so we re-announce on our own every couple of
//! seconds; and when browsing we query every 1.5 s instead of backing off,
//! because AWDL drops most multicast and a backed-off query is a lost one.

use std::collections::HashMap;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use simple_dns::rdata::{AAAA, PTR, RData, SRV, TXT};
use simple_dns::{CLASS, Name, Packet, PacketFlag, QCLASS, QTYPE, Question, ResourceRecord, TYPE};

const SERVICE: &str = "_airdrop._tcp.local";
const GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const PORT: u16 = 5353;
/// Receiver flags in TXT: URL (0x01) | mixed types (0x08) | /Discover (0x80).
const FLAGS: &str = "flags=137";
const ANNOUNCE: Duration = Duration::from_secs(2);
const QUERY: Duration = Duration::from_millis(1500);
/// A phone not heard for this long is gone.
const PEER_TTL: Duration = Duration::from_secs(15);

/// A phone offering AirDrop, as seen on the link.
#[derive(Debug, Clone)]
pub struct Peer {
    pub addr: SocketAddr,
    seen: Instant,
}

/// The interface's link-local address and index, from /proc/net/if_inet6.
pub fn link_local(iface: &str) -> Result<(Ipv6Addr, u32)> {
    let s = std::fs::read_to_string("/proc/net/if_inet6")?;
    for line in s.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // Scope 0x20 = link; flag 0x40 = tentative (DAD still running).
        let tentative = u32::from_str_radix(f.get(4).copied().unwrap_or("0"), 16).unwrap_or(0) & 0x40 != 0;
        if f.len() == 6 && f[5] == iface && f[3] == "20" && !tentative {
            let a = u128::from_str_radix(f[0], 16)?;
            let idx = u32::from_str_radix(f[1], 16)?;
            return Ok((Ipv6Addr::from(a), idx));
        }
    }
    bail!("{iface} non ha un indirizzo link-local (AWDL non è attivo?)")
}

pub struct Mdns {
    sock: UdpSocket,
    ifindex: u32,
    /// `Some` while we are visible.
    me: Mutex<Option<Me>>,
    browsing: AtomicBool,
    peers: Mutex<HashMap<String, Peer>>,
}

struct Me {
    instance: String,
    host: String,
    addr: Ipv6Addr,
    port: u16,
}

impl Mdns {
    pub fn open(iface: &str) -> Result<Arc<Self>> {
        use socket2::{Domain, Protocol, Socket, Type};
        let (_, ifindex) = link_local(iface)?;
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_only_v6(true)?;
        // systemd-resolved or avahi may hold 5353 too.
        s.set_reuse_address(true)?;
        s.set_reuse_port(true)?;
        s.bind_device(Some(iface.as_bytes()))?;
        s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, PORT)).into()).context("porta 5353")?;
        s.join_multicast_v6(&GROUP, ifindex)?;
        s.set_multicast_if_v6(ifindex)?;
        s.set_multicast_hops_v6(255)?;
        s.set_multicast_loop_v6(false)?;
        s.set_read_timeout(Some(Duration::from_millis(250)))?;
        Ok(Arc::new(Self {
            sock: s.into(),
            ifindex,
            me: Mutex::new(None),
            browsing: AtomicBool::new(false),
            peers: Mutex::new(HashMap::new()),
        }))
    }

    /// Becomes visible as `instance` (our service ID) on `addr:port`.
    pub fn announce(&self, instance: &str, host: &str, addr: Ipv6Addr, port: u16) {
        *self.me.lock().unwrap() = Some(Me { instance: instance.into(), host: host.into(), addr, port });
    }

    /// Stops answering and says goodbye (TTL 0) so phones drop us at once.
    pub fn withdraw(&self) {
        if let Some(me) = self.me.lock().unwrap().take() {
            let _ = self.send(&records(&me, 0));
        }
    }

    pub fn browse(&self, on: bool) {
        self.browsing.store(on, Ordering::SeqCst);
        if !on {
            self.peers.lock().unwrap().clear();
        }
    }

    pub fn peers(&self) -> Vec<Peer> {
        let mut p = self.peers.lock().unwrap();
        p.retain(|_, v| v.seen.elapsed() < PEER_TTL);
        p.values().cloned().collect()
    }

    /// The loop; run it on its own thread until `stop` is set.
    pub fn run(&self, stop: &AtomicBool) {
        let mut buf = [0u8; 9000];
        let mut last_announce = Instant::now() - ANNOUNCE;
        let mut last_query = Instant::now() - QUERY;
        while !stop.load(Ordering::SeqCst) {
            if last_announce.elapsed() >= ANNOUNCE {
                if let Some(me) = self.me.lock().unwrap().as_ref() {
                    let _ = self.send(&records(me, 1));
                }
                last_announce = Instant::now();
            }
            if self.browsing.load(Ordering::SeqCst) && last_query.elapsed() >= QUERY {
                let _ = self.send(&query());
                last_query = Instant::now();
            }
            match self.sock.recv_from(&mut buf) {
                Ok((n, src)) => self.handle(&buf[..n], src),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => tracing::debug!("mdns: {e}"),
            }
        }
        self.withdraw();
    }

    fn send(&self, pkt: &[u8]) -> std::io::Result<usize> {
        self.sock.send_to(pkt, SocketAddrV6::new(GROUP, PORT, 0, self.ifindex))
    }

    fn handle(&self, data: &[u8], src: SocketAddr) {
        let Ok(p) = Packet::parse(data) else { return };
        if p.has_flags(PacketFlag::RESPONSE) {
            if self.browsing.load(Ordering::SeqCst) {
                self.learn(&p, src);
            }
            return;
        }
        let me = self.me.lock().unwrap();
        let Some(me) = me.as_ref() else { return };
        let wanted = p.questions.iter().any(|q| {
            let n = q.qname.to_string().to_ascii_lowercase();
            n == SERVICE || n.ends_with(&format!(".{SERVICE}")) || n == format!("{}.local", me.host).to_ascii_lowercase()
        });
        if wanted {
            let _ = self.send(&records(me, 1));
        }
    }

    /// Phones announce `<id>._airdrop._tcp` with an SRV for the port; the
    /// address is simply where the packet came from (link-local on awdl0).
    fn learn(&self, p: &Packet, src: SocketAddr) {
        let SocketAddr::V6(src) = src else { return };
        let all = p.answers.iter().chain(&p.additional_records);
        for rr in all {
            let name = rr.name.to_string();
            if !name.to_ascii_lowercase().ends_with(&format!(".{SERVICE}")) {
                continue;
            }
            let mut peers = self.peers.lock().unwrap();
            if rr.ttl == 0 {
                peers.remove(&name);
                continue;
            }
            if let RData::SRV(srv) = &rr.rdata {
                let addr = SocketAddr::V6(SocketAddrV6::new(*src.ip(), srv.port, 0, self.ifindex));
                let is_ours = self.me.lock().unwrap().as_ref().is_some_and(|m| name.starts_with(&m.instance));
                if !is_ours {
                    let new = !peers.contains_key(&name);
                    peers.insert(name.clone(), Peer { addr, seen: Instant::now() });
                    if new {
                        tracing::info!("trovato {name} su {addr}");
                    }
                }
            } else if let Some(p) = peers.get_mut(&name) {
                p.seen = Instant::now();
            }
        }
    }
}

/// PTR + SRV + TXT + AAAA, as one unsolicited response. `ttl_scale` 0 = goodbye.
fn records(me: &Me, ttl_scale: u32) -> Vec<u8> {
    let inst = format!("{}.{SERVICE}", me.instance);
    let host = format!("{}.local", me.host);
    let mut p = Packet::new_reply(0);
    p.set_flags(PacketFlag::AUTHORITATIVE_ANSWER);
    let txt = TXT::new().with_string(FLAGS).expect("short TXT");
    p.answers.push(ResourceRecord::new(Name::new_unchecked(SERVICE), CLASS::IN, 4500 * ttl_scale, RData::PTR(PTR(Name::new_unchecked(&inst)))));
    p.answers.push(
        ResourceRecord::new(
            Name::new_unchecked(&inst),
            CLASS::IN,
            120 * ttl_scale,
            RData::SRV(SRV { priority: 0, weight: 0, port: me.port, target: Name::new_unchecked(&host) }),
        )
        .with_cache_flush(true),
    );
    p.answers.push(ResourceRecord::new(Name::new_unchecked(&inst), CLASS::IN, 4500 * ttl_scale, RData::TXT(txt)).with_cache_flush(true));
    p.additional_records.push(
        ResourceRecord::new(Name::new_unchecked(&host), CLASS::IN, 120 * ttl_scale, RData::AAAA(AAAA { address: me.addr.into() }))
            .with_cache_flush(true),
    );
    p.build_bytes_vec_compressed().expect("valid records")
}

fn query() -> Vec<u8> {
    let mut p = Packet::new_query(0);
    p.questions.push(Question::new(Name::new_unchecked(SERVICE), QTYPE::TYPE(TYPE::PTR), QCLASS::CLASS(CLASS::IN), false));
    p.build_bytes_vec().expect("valid query")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_records_parse_back() {
        let me = Me { instance: "0a1b2c3d4e5f".into(), host: "nitro".into(), addr: "fe80::1".parse().unwrap(), port: 8771 };
        let raw = records(&me, 1);
        let p = Packet::parse(&raw).unwrap();
        assert!(p.has_flags(PacketFlag::RESPONSE));
        let srv = p.answers.iter().find_map(|r| if let RData::SRV(s) = &r.rdata { Some(s.port) } else { None });
        assert_eq!(srv, Some(8771));
        let txt = p.answers.iter().find_map(|r| if let RData::TXT(t) = &r.rdata { Some(t.attributes()) } else { None });
        assert_eq!(txt.unwrap().get("flags").cloned().flatten().as_deref(), Some("137"));
        assert!(p.answers.iter().all(|r| r.ttl > 0));
        assert!(Packet::parse(&records(&me, 0)).unwrap().answers.iter().all(|r| r.ttl == 0));
    }

    #[test]
    fn query_is_ptr() {
        let raw = query();
        let p = Packet::parse(&raw).unwrap();
        assert_eq!(p.questions[0].qname.to_string(), SERVICE);
    }
}
