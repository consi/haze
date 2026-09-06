//! Bounded ICMP tracing, using separate sockets from surge-ping.
use crate::dns::DnsResolvers;
use anyhow::Result;
use dashmap::DashMap;
use futures::{StreamExt, stream::FuturesUnordered};
use haze_store::{MetadataRecord, MetadataStore};
use serde_json::{Value, json};
use socket2::{Domain, Protocol, SockRef, Socket, Type};
use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::UdpSocket,
    sync::{Mutex, oneshot},
};
use uuid::Uuid;

const HOPS: u16 = 32;
const ROUNDS: u16 = 5;
#[derive(Default)]
struct Hop {
    sent: u32,
    received: u32,
    total_ms: f64,
    addresses: BTreeSet<IpAddr>,
}

pub async fn collect(
    host: Uuid,
    target: IpAddr,
    resolvers: Arc<DnsResolvers>,
    store: Arc<MetadataStore>,
    window: u32,
    timeout: Duration,
    reply_timeout: Duration,
) -> Result<()> {
    let started = chrono::Utc::now().timestamp();
    // Keep measurements outside the cancellable future: a deadline or socket
    // error must not erase routers that already answered.
    let (progress, error) = capture_with_deadline(timeout, async |progress: &mut TraceProgress| {
        trace_into(
            target,
            reply_timeout,
            &mut progress.hops,
            &mut progress.reached,
        )
        .await
    })
    .await;
    let TraceProgress { mut hops, reached } = progress;
    // Do not invent measurements for TTLs that were never attempted.
    let attempted = hops.iter().rposition(|h| h.sent > 0).map_or(0, |i| i + 1);
    hops.truncate(attempted);
    let resolver = resolvers.get(None);
    let path = enrich_path(
        &hops,
        |ip| {
            let resolver = resolver.clone();
            async move {
                match tokio::time::timeout(Duration::from_millis(250), resolver.reverse_lookup(ip))
                    .await
                {
                    Ok(Ok(answer)) => answer.answers().iter().find_map(|r| match &r.data {
                        hickory_resolver::proto::rr::RData::PTR(name) => {
                            Some(name.to_utf8().trim_end_matches('.').to_owned())
                        }
                        _ => None,
                    }),
                    _ => None,
                }
            }
        },
        Duration::from_secs(1),
    )
    .await;
    let context = json!({"target":target.to_string(), "hops":path});
    let previous = store.read_checkpoint(host, "trace-state")?;
    let old = previous.get("context").cloned().unwrap_or(Value::Null);
    let event = classify(&old, &context, error.is_some(), reached);
    let measurements: Vec<_> = hops.iter().map(|h|json!({"sent":h.sent,"received":h.received,"loss_pct":if h.sent==0 {0.0} else {100.0*(1.0-f64::from(h.received)/f64::from(h.sent))},"avg_ms":if h.received==0 {None} else {Some(h.total_ms/f64::from(h.received))}})).collect();
    let finished = chrono::Utc::now().timestamp();
    let record = MetadataRecord::new(
        host,
        finished,
        "trace",
        context.clone(),
        json!({"event":event,"started":started,"finished":finished,"previous_observed":previous.get("timestamp"),"previous_id":previous.get("id"),"reached":reached,"error":error,"hops":measurements}),
    );
    let state = json!({"context":context,"timestamp":finished,"id":record.id});
    tokio::task::spawn_blocking(move || -> Result<()> {
        store.append_local(record, window)?;
        if error.is_none() && reached {
            store.checkpoint(host, "trace-state", &state)?;
        }
        Ok(())
    })
    .await??;
    Ok(())
}
struct TraceProgress {
    hops: Vec<Hop>,
    reached: bool,
}

async fn capture_with_deadline<F>(timeout: Duration, capture: F) -> (TraceProgress, Option<String>)
where
    F: AsyncFnOnce(&mut TraceProgress) -> Result<()>,
{
    let mut progress = TraceProgress {
        hops: (0..HOPS).map(|_| Hop::default()).collect(),
        reached: false,
    };
    let error = match tokio::time::timeout(timeout, capture(&mut progress)).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("Trace deadline exceeded; partial observations retained".into()),
    };
    (progress, error)
}

// PTR is optional enrichment. Preserve every numeric address, even when
// lookups fail or the shared budget expires; deduplicate repeated addresses.
async fn enrich_path<F, Fut>(hops: &[Hop], lookup: F, budget: Duration) -> Vec<Vec<Value>>
where
    F: Fn(IpAddr) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let addresses: BTreeSet<_> = hops
        .iter()
        .flat_map(|h| h.addresses.iter().copied())
        .collect();
    let mut names = std::collections::BTreeMap::new();
    let lookups = futures::stream::iter(addresses)
        .map(|ip| {
            let future = lookup(ip);
            async move { (ip, future.await) }
        })
        .buffer_unordered(8);
    let _ = tokio::time::timeout(budget, async {
        tokio::pin!(lookups);
        while let Some((ip, dns)) = lookups.next().await {
            names.insert(ip, dns);
        }
    })
    .await;
    hops.iter()
        .map(|hop| {
            hop.addresses
                .iter()
                .map(|ip| json!({"ip":ip.to_string(),"dns":names.get(ip).and_then(Clone::clone)}))
                .collect()
        })
        .collect()
}

fn ips(context: &Value) -> Vec<Vec<String>> {
    context
        .get("hops")
        .and_then(Value::as_array)
        .map(|hops| {
            hops.iter()
                .map(|h| {
                    h.as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|a| {
                                    a.get("ip").and_then(Value::as_str).map(str::to_owned)
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}
fn classify(old: &Value, new: &Value, failed: bool, reached: bool) -> &'static str {
    if failed {
        return if ips(new).iter().all(Vec::is_empty) {
            "trace_failed"
        } else {
            "incomplete"
        };
    }
    if !reached {
        return "incomplete";
    }
    if old.is_null() || old.get("target") != new.get("target") {
        return "baseline";
    }
    let a = ips(old);
    let b = ips(new);
    if a == b {
        return "";
    }
    if a.iter()
        .zip(&b)
        .any(|(x, y)| !x.is_empty() && !y.is_empty() && x != y)
        || a.len() != b.len()
    {
        "route_changed"
    } else {
        "visibility_changed"
    }
}
async fn trace_into(
    target: IpAddr,
    reply_timeout: Duration,
    hops: &mut Vec<Hop>,
    reached: &mut bool,
) -> Result<()> {
    let client = TraceClient::get(target.is_ipv4())?;
    let ident = TRACE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut destination_hop = HOPS;
    for round in 0..ROUNDS {
        let mut pending = FuturesUnordered::new();
        let mut ttl = 1u16;
        let mut send_tick = tokio::time::interval(Duration::from_millis(5));
        send_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                result=pending.next(), if !pending.is_empty() => {
                    if let Some((hop_ttl,result))=result
                        && let Some((source,elapsed,echo))=result? {
                            let hop=&mut hops[usize::from(hop_ttl-1)];hop.received+=1;hop.total_ms+=elapsed;hop.addresses.insert(source);
                            if echo {destination_hop=destination_hop.min(hop_ttl);*reached=true;}
                    }
                }
                _=send_tick.tick(), if ttl<=destination_hop => {
                    let client=client.clone();let hop_ttl=ttl;let seq=round*HOPS+ttl;
                    pending.push(async move {(hop_ttl,client.probe(target,ident,seq,hop_ttl,reply_timeout).await)});
                    hops[usize::from(ttl-1)].sent+=1;ttl+=1;
                }
            }
            if ttl > destination_hop && pending.is_empty() {
                break;
            }
        }
    }
    hops.truncate(usize::from(destination_hop));
    Ok(())
}

type ProbeKey = (IpAddr, u16, u16);
type Reply = (IpAddr, Instant, bool);
struct TraceClient {
    socket: Arc<UdpSocket>,
    sending: Mutex<()>,
    pending: DashMap<ProbeKey, oneshot::Sender<Reply>>,
}
static CLIENTS: std::sync::OnceLock<std::sync::Mutex<Vec<Arc<TraceClient>>>> =
    std::sync::OnceLock::new();
static TRACE_ID: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(32768);
impl TraceClient {
    fn get(v4: bool) -> Result<Arc<Self>> {
        let mut clients = CLIENTS
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = clients
            .iter()
            .find(|c| c.socket.local_addr().is_ok_and(|a| a.is_ipv4() == v4))
        {
            return Ok(client.clone());
        }
        let socket = Socket::new(
            if v4 { Domain::IPV4 } else { Domain::IPV6 },
            Type::RAW,
            Some(if v4 {
                Protocol::ICMPV4
            } else {
                Protocol::ICMPV6
            }),
        )?;
        socket.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(socket.into())?);
        let client = Arc::new(Self {
            socket: socket.clone(),
            sending: Mutex::new(()),
            pending: DashMap::new(),
        });
        let weak = Arc::downgrade(&client);
        tokio::spawn(async move {
            let mut bytes = [0u8; 2048];
            loop {
                let (n, source) = match socket.recv_from(&mut bytes).await {
                    Ok(value) => value,
                    Err(e) => {
                        tracing::warn!(error=%e,"trace socket receive failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let received = Instant::now();
                let Some(client) = weak.upgrade() else { break };
                if let Some((target, id, seq)) = reply_key(&bytes[..n], source.ip())
                    && let Some((_, echo)) = reply(&bytes[..n], target, source.ip(), id)
                    && let Some((_, sender)) = client.pending.remove(&(target, id, seq))
                {
                    let _ = sender.send((source.ip(), received, echo));
                }
            }
        });
        clients.push(client.clone());
        Ok(client)
    }
    async fn probe(
        self: Arc<Self>,
        target: IpAddr,
        id: u16,
        seq: u16,
        ttl: u16,
        reply_timeout: Duration,
    ) -> Result<Option<(IpAddr, f64, bool)>> {
        let (tx, rx) = oneshot::channel();
        let key = (target, id, seq);
        self.pending.insert(key, tx);
        let _cleanup = PendingProbe {
            client: self.clone(),
            key,
        };
        let mut packet = [0u8; 24];
        packet[0] = if target.is_ipv4() { 8 } else { 128 };
        packet[4..6].copy_from_slice(&id.to_be_bytes());
        packet[6..8].copy_from_slice(&seq.to_be_bytes());
        packet[8..16].copy_from_slice(b"HAZE-TRC");
        if target.is_ipv4() {
            let sum = checksum(&packet);
            packet[2..4].copy_from_slice(&sum.to_be_bytes());
        }
        // Serialize only TTL selection and send, never the response wait. This
        // avoids per-target sockets while preserving per-packet hop limits.
        let send_guard = self.sending.lock().await;
        let sock = SockRef::from(self.socket.as_ref());
        if target.is_ipv4() {
            sock.set_ttl_v4(u32::from(ttl))?;
        } else {
            sock.set_unicast_hops_v6(u32::from(ttl))?;
        }
        let started = Instant::now();
        self.socket
            .send_to(&packet, SocketAddr::new(target, 0))
            .await?;
        drop(send_guard);
        match tokio::time::timeout(reply_timeout, rx).await {
            Ok(Ok((source, time, echo))) => Ok(Some((
                source,
                time.saturating_duration_since(started).as_secs_f64() * 1000.0,
                echo,
            ))),
            _ => Ok(None),
        }
    }
}
struct PendingProbe {
    client: Arc<TraceClient>,
    key: ProbeKey,
}
impl Drop for PendingProbe {
    fn drop(&mut self) {
        self.client.pending.remove(&self.key);
    }
}
fn reply_key(bytes: &[u8], source: IpAddr) -> Option<ProbeKey> {
    let v4 = source.is_ipv4();
    let packet = if v4 {
        if bytes.len() < 20 || bytes[0] >> 4 != 4 {
            return None;
        }
        bytes.get(usize::from(bytes[0] & 15) * 4..)?
    } else {
        bytes
    };
    if packet.len() < 8 {
        return None;
    }
    let echo = packet[0] == if v4 { 0 } else { 129 };
    let (target, payload) = if echo {
        if packet.get(8..16) != Some(b"HAZE-TRC".as_slice()) {
            return None;
        }
        (source, packet)
    } else {
        if !matches!((v4, packet[0]), (true, 11 | 3) | (false, 3 | 1)) {
            return None;
        }
        let quoted = packet.get(8..)?;
        let target = if v4 {
            let dst = quoted.get(16..20)?;
            IpAddr::V4(Ipv4Addr::new(dst[0], dst[1], dst[2], dst[3]))
        } else {
            IpAddr::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(quoted.get(24..40)?).ok()?,
            ))
        };
        (target, ip_payload(quoted, target)?)
    };
    if payload.len() < 8 {
        return None;
    }
    Some((
        target,
        u16::from_be_bytes([payload[4], payload[5]]),
        u16::from_be_bytes([payload[6], payload[7]]),
    ))
}
fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = bytes
        .chunks(2)
        .map(|b| u32::from(u16::from_be_bytes([b[0], *b.get(1).unwrap_or(&0)])))
        .sum();
    while sum >> 16 != 0 {
        sum = (sum & 65535) + (sum >> 16);
    }
    !(sum as u16)
}
fn ip_payload(bytes: &[u8], target: IpAddr) -> Option<&[u8]> {
    if target.is_ipv4() {
        if bytes.len() < 20 || bytes[0] >> 4 != 4 || bytes[9] != 1 {
            return None;
        }
        if IpAddr::V4(Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19])) != target {
            return None;
        }
        let len = usize::from(bytes[0] & 15) * 4;
        if len < 20 {
            return None;
        }
        bytes.get(len..)
    } else {
        if bytes.len() < 40 || bytes[0] >> 4 != 6 {
            return None;
        }
        if IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[24..40]).ok()?)) != target {
            return None;
        }
        let mut next = bytes[6];
        let mut offset = 40;
        for _ in 0..8 {
            if next == 58 {
                return bytes.get(offset..);
            }
            let h = bytes.get(offset..)?;
            if h.len() < 2 {
                return None;
            }
            let len = match next {
                0 | 43 | 60 => (usize::from(h[1]) + 1) * 8,
                44 => 8,
                _ => return None,
            };
            next = h[0];
            offset += len;
        }
        None
    }
}
fn reply(bytes: &[u8], target: IpAddr, source: IpAddr, ident: u16) -> Option<(u16, bool)> {
    let v4 = target.is_ipv4();
    let bytes = if v4 {
        if bytes.len() < 20 || bytes[0] >> 4 != 4 {
            return None;
        }
        let n = usize::from(bytes[0] & 15) * 4;
        if n < 20 {
            return None;
        }
        bytes.get(n..)?
    } else {
        bytes
    };
    if bytes.len() < 8 {
        return None;
    }
    let echo = bytes[0] == if v4 { 0 } else { 129 };
    let payload = if echo {
        if source != target || bytes[1] != 0 {
            return None;
        }
        bytes
    } else {
        if !matches!((v4, bytes[0]), (true, 11 | 3) | (false, 3 | 1)) {
            return None;
        }
        let p = ip_payload(bytes.get(8..)?, target)?;
        if p.first().copied() != Some(if v4 { 8 } else { 128 }) {
            return None;
        }
        p
    };
    if payload.len() < 8 || u16::from_be_bytes([payload[4], payload[5]]) != ident {
        return None;
    }
    let seq = u16::from_be_bytes([payload[6], payload[7]]);
    if seq == 0 || seq > HOPS * ROUNDS {
        return None;
    }
    Some((seq, echo))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "Requires CAP_NET_RAW; loopback only"]
    async fn live_loopback() {
        for target in ["127.0.0.1", "::1"] {
            let mut hops: Vec<Hop> = (0..HOPS).map(|_| Hop::default()).collect();
            let mut reached = false;
            trace_into(
                target.parse().unwrap(),
                Duration::from_secs(2),
                &mut hops,
                &mut reached,
            )
            .await
            .unwrap();
            assert!(reached);
            assert_eq!(hops.len(), 1);
            assert_eq!(hops[0].received, 5);
            eprintln!("{target}: {:.3} ms", hops[0].total_ms / 5.0);
        }
    }
    #[tokio::test]
    async fn ptr_failure_and_budget_expiry_preserve_addresses() {
        let hops = vec![Hop {
            addresses: [
                "192.0.2.1".parse().unwrap(),
                "192.0.2.2".parse().unwrap(),
                "192.0.2.3".parse().unwrap(),
            ]
            .into(),
            ..Hop::default()
        }];
        let path = enrich_path(
            &hops,
            |ip| async move {
                if ip == "192.0.2.1".parse::<IpAddr>().unwrap() {
                    Some("router.example".into())
                } else if ip == "192.0.2.2".parse::<IpAddr>().unwrap() {
                    None
                } else {
                    std::future::pending().await
                }
            },
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(path[0].len(), 3);
        assert_eq!(path[0][0]["dns"], "router.example");
        assert_eq!(path[0][1]["ip"], "192.0.2.2");
        assert!(path[0][1]["dns"].is_null());
        assert_eq!(path[0][2]["ip"], "192.0.2.3");
        assert!(path[0][2]["dns"].is_null());
    }

    #[test]
    fn partial_routes_are_reported_even_on_first_capture_or_deadline() {
        let partial = json!({"target":"192.0.2.9","hops":[[{"ip":"192.0.2.1"}],[]]});
        assert_eq!(classify(&Value::Null, &partial, false, false), "incomplete");
        assert_eq!(classify(&Value::Null, &partial, true, false), "incomplete");
        assert_eq!(classify(&Value::Null, &partial, true, true), "incomplete");
        assert_eq!(
            classify(&Value::Null, &json!({"hops":[]}), true, false),
            "trace_failed"
        );
    }

    #[tokio::test]
    async fn deadline_retains_transit_observations() {
        let (progress, error) = capture_with_deadline(
            Duration::from_millis(10),
            async |progress: &mut TraceProgress| {
                progress.hops[0].sent = 1;
                progress.hops[0].received = 1;
                progress.hops[0]
                    .addresses
                    .insert("192.0.2.1".parse().unwrap());
                std::future::pending().await
            },
        )
        .await;
        assert!(error.unwrap().contains("deadline"));
        assert!(!progress.reached);
        assert_eq!(progress.hops[0].received, 1);
        assert!(
            progress.hops[0]
                .addresses
                .contains(&"192.0.2.1".parse().unwrap())
        );
    }

    #[test]
    fn dns_does_not_change_route() {
        let a = json!({"target":"1","hops":[[{"ip":"2","dns":"old"}]]});
        let b = json!({"target":"1","hops":[[{"ip":"2","dns":"new"}]]});
        assert_eq!(classify(&a, &b, false, true), "");
        assert_eq!(
            classify(&a, &json!({"target":"1","hops":[[]]}), false, false),
            "incomplete"
        );
    }
    #[test]
    fn parses_quoted_v4_and_rejects_wrong_destination() {
        let target: IpAddr = "1.2.3.4".parse().unwrap();
        let router = "5.6.7.8".parse().unwrap();
        let mut p = vec![0; 56];
        p[0] = 0x45;
        p[20] = 11;
        p[28] = 0x45;
        p[37] = 1;
        p[44..48].copy_from_slice(&[1, 2, 3, 4]);
        p[48] = 8;
        p[52..54].copy_from_slice(&123u16.to_be_bytes());
        p[54..56].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(reply(&p, target, router, 123), Some((2, false)));
        assert_eq!(reply(&p, target, router, 124), None);
        p[47] = 5;
        assert_eq!(reply(&p, target, router, 123), None);
    }
    #[test]
    fn truncated_packets_do_not_panic() {
        for n in 0..100 {
            assert!(
                reply(
                    &vec![0; n],
                    "::1".parse().unwrap(),
                    "::1".parse().unwrap(),
                    1
                )
                .is_none()
            );
        }
    }
}
