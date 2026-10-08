use crosscopy_core::{ClipItem, Message};
use crosscopy_net::{Identity, Incoming, Node, NodeOptions, Peer};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::time::{sleep, timeout};

/// Address nothing listens on, for peers that should only be dialed *by*.
const NOWHERE: &str = "127.0.0.1:1";

fn identity() -> (Identity, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Identity::load_or_create(dir.path()).unwrap(), dir)
}

fn peer(identity: &Identity, name: &str, address: impl Into<String>) -> Peer {
    Peer { id: identity.id, name: name.into(), address: Some(address.into()) }
}

fn start(identity: &Identity, port: u16, peers: Vec<Peer>) -> (Node, Receiver<Incoming>) {
    let options = NodeOptions {
        listen: SocketAddr::from(([127, 0, 0, 1], port)),
        device_name: "test".into(),
        discovery: false,
    };
    Node::start(identity, options, peers).unwrap()
}

fn clip(identity: &Identity, text: &str) -> Message {
    Message::Clip(ClipItem::from_text(identity.id, text))
}

/// Broadcasts until at least one peer is connected to receive it.
async fn broadcast_when_connected(node: &Node, message: &Message) {
    timeout(Duration::from_secs(10), async {
        while node.broadcast(message) == 0 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("peer never connected");
}

async fn next(inbox: &mut Receiver<Incoming>) -> Incoming {
    timeout(Duration::from_secs(5), inbox.recv())
        .await
        .expect("timed out waiting for message")
        .expect("inbox closed")
}

#[tokio::test]
async fn paired_nodes_exchange_clips_both_ways() {
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();

    let (node_b, mut inbox_b) = start(&b, 0, vec![peer(&a, "a", NOWHERE)]);
    let b_addr = node_b.local_addr().unwrap().to_string();
    let (node_a, mut inbox_a) = start(&a, 0, vec![peer(&b, "b", b_addr)]);

    let from_a = clip(&a, "hello from a");
    broadcast_when_connected(&node_a, &from_a).await;
    let got = next(&mut inbox_b).await;
    assert_eq!(got.from, a.id);
    assert_eq!(got.from_name, "a");
    assert_eq!(got.message, from_a);

    let from_b = clip(&b, "hello from b");
    broadcast_when_connected(&node_b, &from_b).await;
    let got = next(&mut inbox_a).await;
    assert_eq!(got.from, b.id);
    assert_eq!(got.message, from_b);

    // Order is preserved on a connection.
    for i in 0..10 {
        node_a.broadcast(&clip(&a, &i.to_string()));
    }
    for i in 0..10 {
        let Message::Clip(item) = next(&mut inbox_b).await.message else {
            panic!("expected a clip");
        };
        assert_eq!(item.text(), Some(i.to_string().as_str()));
    }
}

#[tokio::test]
async fn mutual_dialing_settles_on_one_working_connection() {
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();
    let (node_a, mut inbox_a) = start(&a, 0, vec![]);
    let (node_b, mut inbox_b) = start(&b, 0, vec![]);
    let a_addr = node_a.local_addr().unwrap().to_string();
    let b_addr = node_b.local_addr().unwrap().to_string();
    node_a.set_peers(vec![peer(&b, "b", b_addr)]);
    node_b.set_peers(vec![peer(&a, "a", a_addr)]);

    // Let both dials land and the duplicate get retired.
    broadcast_when_connected(&node_a, &clip(&a, "warmup")).await;
    next(&mut inbox_b).await;
    sleep(Duration::from_secs(4)).await;

    let (from_a, from_b) = (clip(&a, "after dedupe a"), clip(&b, "after dedupe b"));
    assert_eq!(node_a.broadcast(&from_a), 1);
    assert_eq!(node_b.broadcast(&from_b), 1);
    assert_eq!(next(&mut inbox_b).await.message, from_a);
    assert_eq!(next(&mut inbox_a).await.message, from_b);
}

#[tokio::test]
async fn unpinned_device_is_rejected() {
    let (b, _b_dir) = identity();
    let (stranger, _s_dir) = identity();
    let (trusted, _t_dir) = identity();

    // B only trusts `trusted`, not `stranger`.
    let (node_b, _inbox_b) = start(&b, 0, vec![peer(&trusted, "t", NOWHERE)]);
    let b_addr = node_b.local_addr().unwrap().to_string();
    let (node_s, _inbox_s) = start(&stranger, 0, vec![peer(&b, "b", b_addr)]);

    sleep(Duration::from_secs(2)).await;
    let msg = clip(&stranger, "let me in");
    assert_eq!(node_s.broadcast(&msg), 0, "stranger should not be connected");
    assert_eq!(node_b.broadcast(&msg), 0, "b should not have accepted the stranger");
}

#[tokio::test]
async fn impersonating_a_pinned_peer_fails() {
    // The client expects `b`, but `imposter` answers at that address.
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();
    let (imposter, _i_dir) = identity();

    let (node_i, _inbox_i) = start(&imposter, 0, vec![peer(&a, "a", NOWHERE)]);
    let i_addr = node_i.local_addr().unwrap().to_string();
    let (node_a, _inbox_a) = start(&a, 0, vec![peer(&b, "b", i_addr)]);

    sleep(Duration::from_secs(2)).await;
    assert_eq!(node_a.broadcast(&clip(&a, "secret")), 0, "a must not connect to an imposter");
}

#[tokio::test]
async fn peers_can_be_added_and_removed_at_runtime() {
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();

    // Neither knows the other at first.
    let (node_b, mut inbox_b) = start(&b, 0, vec![]);
    let b_addr = node_b.local_addr().unwrap().to_string();
    let (node_a, _inbox_a) = start(&a, 0, vec![]);

    node_b.set_peers(vec![peer(&a, "a", NOWHERE)]);
    node_a.set_peers(vec![peer(&b, "b", b_addr)]);
    broadcast_when_connected(&node_a, &clip(&a, "now paired")).await;
    assert_eq!(next(&mut inbox_b).await.from_name, "a");

    // Unpairing on B drops the connection and keeps A out.
    node_b.set_peers(vec![]);
    sleep(Duration::from_secs(3)).await;
    assert_eq!(node_a.broadcast(&clip(&a, "still there?")), 0);
    assert!(node_b.connected_peers().is_empty());
}
