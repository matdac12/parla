//! Remembers the provider APIs' addresses so a slow DNS server never sits on
//! the hotkey-to-paste path (PARLA-108).
//!
//! A client VPN can push a DNS server that takes ~12 s to answer before
//! Windows falls back to the next one, and the 5 s connect timeout counts the
//! lookup: the dictation failed, and a retry worked once the slow answer was
//! cached. Here a host is looked up once, at startup or on a provider switch,
//! and its addresses are reused from memory from then on. They are looked up
//! again only after a connection to them fails, in the background, keeping
//! the old ones until an answer arrives.
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};
use tokio::sync::watch;

type Lookup = Box<dyn Fn(&str) -> io::Result<Vec<IpAddr>> + Send + Sync>;
/// None while the lookup runs.
type Outcome = Option<Result<Vec<IpAddr>, String>>;

#[derive(Default)]
struct State {
    known: HashMap<String, Vec<IpAddr>>,
    running: HashMap<String, watch::Receiver<Outcome>>,
}

pub struct CachingResolver {
    lookup: Lookup,
    state: Mutex<State>,
}

/// The cache every provider client shares, as `ClientBuilder::dns_resolver`
/// takes it.
pub fn resolver() -> Arc<Cached> {
    Arc::new(Cached(shared()))
}

/// The process-wide cache behind a type of this crate, so it can implement
/// reqwest's `Resolve`.
pub struct Cached(Arc<CachingResolver>);

fn shared() -> Arc<CachingResolver> {
    static RESOLVER: OnceLock<Arc<CachingResolver>> = OnceLock::new();
    RESOLVER
        .get_or_init(|| CachingResolver::new(Box::new(system_lookup)))
        .clone()
}

/// Looks up the provider's API host in the background unless it is known or
/// being looked up. Never blocks.
pub fn warm_provider(provider_id: &str) {
    if let Some(host) = api_host(provider_id) {
        let _ = shared().addresses(host);
    }
}

/// A connection to `host` failed: look it up again in the background, keeping
/// the known addresses until the answer arrives.
pub fn refresh(host: &str) {
    let resolver = shared();
    let mut state = resolver.state();
    resolver.start(&mut state, host);
}

fn api_host(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "openai" => Some(crate::provider::openai::HOST),
        "xai" => Some(crate::provider::xai::HOST),
        _ => None,
    }
}

impl CachingResolver {
    fn new(lookup: Lookup) -> Arc<Self> {
        Arc::new(Self {
            lookup,
            state: Mutex::default(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The known addresses, or the running lookup to wait for.
    fn addresses(self: &Arc<Self>, host: &str) -> Result<Vec<IpAddr>, watch::Receiver<Outcome>> {
        let mut state = self.state();
        match state.known.get(host) {
            Some(addrs) => Ok(addrs.clone()),
            None => Err(self.start(&mut state, host)),
        }
    }

    /// Starts a lookup on its own thread unless one is running for `host`.
    fn start(self: &Arc<Self>, state: &mut State, host: &str) -> watch::Receiver<Outcome> {
        if let Some(running) = state.running.get(host) {
            return running.clone();
        }
        let (sender, receiver) = watch::channel(None);
        let (resolver, name) = (self.clone(), host.to_owned());
        let spawned = std::thread::Builder::new()
            .name("parla-dns".into())
            .spawn(move || resolver.finish(&name, sender));
        if spawned.is_ok() {
            state.running.insert(host.to_owned(), receiver.clone());
        }
        // A failed spawn drops the sender: the waiter sees the channel close.
        receiver
    }

    fn finish(&self, host: &str, sender: watch::Sender<Outcome>) {
        let outcome = (self.lookup)(host)
            .and_then(|addrs| {
                if addrs.is_empty() {
                    Err(io::Error::new(io::ErrorKind::NotFound, "no addresses"))
                } else {
                    Ok(addrs)
                }
            })
            .map_err(|error| error.to_string());
        let mut state = self.state();
        // A failed lookup keeps the last good addresses.
        if let Ok(addrs) = &outcome {
            state.known.insert(host.to_owned(), addrs.clone());
        }
        state.running.remove(host);
        drop(state);
        sender.send_replace(Some(outcome));
    }
}

impl Resolve for Cached {
    fn resolve(&self, name: Name) -> Resolving {
        let found = self.0.addresses(name.as_str());
        Box::pin(async move {
            let addrs = match found {
                Ok(addrs) => addrs,
                Err(mut running) => {
                    let outcome = running
                        .wait_for(Option::is_some)
                        .await
                        .map_err(|_| "DNS lookup could not start")?
                        .clone();
                    outcome.unwrap_or_else(|| Err("DNS lookup ended without an answer".into()))?
                }
            };
            // Port 0: the connector puts the URL's port in.
            let addrs: Addrs = Box::new(addrs.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

/// The operating system's lookup, in its order, without duplicates.
fn system_lookup(host: &str) -> io::Result<Vec<IpAddr>> {
    let mut addrs = Vec::new();
    for addr in (host, 0).to_socket_addrs()? {
        if !addrs.contains(&addr.ip()) {
            addrs.push(addr.ip());
        }
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{Ipv4Addr, TcpListener},
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
    };

    const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const OTHER: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

    fn wait(running: watch::Receiver<Outcome>) -> Outcome {
        let mut running = running;
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async move { running.wait_for(Option::is_some).await.unwrap().clone() })
    }

    /// Answers lookup N with `answer(N)`; lookup 0 waits for the gate when
    /// `gated`. Returns the resolver, the lookup count and the gate.
    fn scripted(
        gated: bool,
        answer: fn(usize) -> io::Result<Vec<IpAddr>>,
    ) -> (Arc<CachingResolver>, Arc<AtomicUsize>, mpsc::Sender<()>) {
        let (gate, opened) = mpsc::channel::<()>();
        let opened = Mutex::new(opened);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let resolver = CachingResolver::new(Box::new(move |_| {
            let call = counted.fetch_add(1, Ordering::SeqCst);
            if gated && call == 0 {
                opened.lock().unwrap().recv().unwrap();
            }
            answer(call)
        }));
        (resolver, calls, gate)
    }

    fn timed_out() -> io::Error {
        io::Error::new(io::ErrorKind::TimedOut, "timed out")
    }

    #[test]
    fn a_host_is_looked_up_once_and_then_answered_from_memory() {
        let (resolver, calls, _) = scripted(false, |_| Ok(vec![LOCAL]));
        let running = resolver.addresses("api.example").unwrap_err();
        assert_eq!(wait(running), Some(Ok(vec![LOCAL])));
        for _ in 0..3 {
            assert_eq!(resolver.addresses("api.example").unwrap(), vec![LOCAL]);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_refresh_keeps_the_known_addresses_until_an_answer_arrives() {
        let (resolver, calls, _) = scripted(false, |call| match call {
            0 => Ok(vec![LOCAL]),
            // The slow DNS server: the refresh times out.
            1 => Err(timed_out()),
            _ => Ok(vec![OTHER]),
        });
        wait(resolver.addresses("api.example").unwrap_err());
        let refresh = |resolver: &Arc<CachingResolver>| {
            let mut state = resolver.state();
            resolver.start(&mut state, "api.example")
        };
        // The failed refresh kept the last good addresses.
        assert!(matches!(wait(refresh(&resolver)), Some(Err(_))));
        assert_eq!(resolver.addresses("api.example").unwrap(), vec![LOCAL]);
        // A refresh that answers replaces them.
        assert_eq!(wait(refresh(&resolver)), Some(Ok(vec![OTHER])));
        assert_eq!(resolver.addresses("api.example").unwrap(), vec![OTHER]);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn waiters_share_one_lookup_and_failures_are_not_cached() {
        let (resolver, calls, gate) = scripted(true, |call| match call {
            0 => Err(timed_out()),
            1 => Ok(vec![]),
            _ => Ok(vec![LOCAL]),
        });
        let first = resolver.addresses("api.example").unwrap_err();
        let second = resolver.addresses("api.example").unwrap_err();
        gate.send(()).unwrap();
        assert!(matches!(wait(first), Some(Err(_))));
        assert!(matches!(wait(second), Some(Err(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // An empty answer is a failure too.
        let empty = resolver.addresses("api.example").unwrap_err();
        assert_eq!(wait(empty), Some(Err("no addresses".into())));
        let found = resolver.addresses("api.example").unwrap_err();
        assert_eq!(wait(found), Some(Ok(vec![LOCAL])));
    }

    #[test]
    fn a_client_connects_through_the_cache_with_the_urls_port() {
        let listener = TcpListener::bind((LOCAL, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .unwrap();
        });
        let (resolver, calls, _) = scripted(false, |_| Ok(vec![LOCAL]));
        let client = reqwest::blocking::Client::builder()
            .dns_resolver(Arc::new(Cached(resolver)))
            .build()
            .unwrap();
        let body = client
            .get(format!("http://api.example:{port}/"))
            .send()
            .unwrap()
            .text()
            .unwrap();
        assert_eq!(body, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
