use crosscopy_net::Identity;
use crosscopy_net::pairing::{Candidate, Pairing, PairingEvent, PendingPair};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;

fn identity() -> (Identity, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Identity::load_or_create(dir.path()).unwrap(), dir)
}

struct Session {
    on_a: PendingPair,
    on_b: PendingPair,
    // Dropping a `Pairing` closes its endpoint, so keep both alive.
    _endpoints: (Pairing, Pairing),
}

/// Connects `a` to `b` without mDNS and returns both pending sessions.
async fn handshake(a: &Identity, b: &Identity) -> Session {
    let pa = Pairing::start_with(a, "alpha", false).unwrap();
    let mut pb = Pairing::start_with(b, "bravo", false).unwrap();
    let port = pb.local_addr().unwrap().port();
    let candidate = Candidate {
        id: b.id,
        name: "bravo".into(),
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], port))],
    };

    let initiated = pa.connect(&candidate).await.unwrap();
    let responded = match timeout(Duration::from_secs(5), pb.next()).await.unwrap() {
        Some(PairingEvent::Incoming(p)) => p,
        _ => panic!("expected an incoming pairing"),
    };
    Session { on_a: initiated, on_b: responded, _endpoints: (pa, pb) }
}

#[tokio::test]
async fn both_sides_see_the_same_code_and_identities() {
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();
    let Session { on_a, on_b, _endpoints } = handshake(&a, &b).await;

    assert_eq!(on_a.code, on_b.code);
    assert_eq!(on_a.peer_id, b.id);
    assert_eq!(on_b.peer_id, a.id);
    assert_eq!(on_a.peer_name, "bravo");
    assert_eq!(on_b.peer_name, "alpha");

    let (ra, rb) = tokio::join!(on_a.finish(true), on_b.finish(true));
    assert!(ra.unwrap());
    assert!(rb.unwrap());
}

#[tokio::test]
async fn one_side_declining_cancels_for_both() {
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();
    let Session { on_a, on_b, _endpoints } = handshake(&a, &b).await;

    let (ra, rb) = tokio::join!(on_a.finish(true), on_b.finish(false));
    assert!(!ra.unwrap());
    assert!(!rb.unwrap());
}

#[tokio::test]
async fn codes_differ_between_sessions() {
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();
    let mut codes = std::collections::HashSet::new();
    for _ in 0..3 {
        codes.insert(handshake(&a, &b).await.on_a.code);
    }
    assert!(codes.len() > 1, "fresh nonces should give fresh codes");
}
