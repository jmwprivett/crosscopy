use crosscopy_core::{ClipItem, Message};
use crosscopy_net::{Identity, Incoming, Node, Peer};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::time::{sleep, timeout};

const LOCALHOST: ([u8; 4], u16) = ([127, 0, 0, 1], 0);
/// Address nothing listens on, for peers that should only be dialed *by*.
const NOWHERE: &str = "127.0.0.1:1";

fn identity() -> (Identity, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Identity::load_or_create(dir.path()).unwrap(), dir)
}

fn peer(identity: &Identity, name: &str, address: impl Into<String>) -> Peer {
    Peer { id: identity.id, name: name.into(), address: address.into() }
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

    let (node_b, mut inbox_b) = Node::start(&b, LOCALHOST.into(), vec![peer(&a, "a", NOWHERE)]).unwrap();
    let b_addr: SocketAddr = node_b.local_addr().unwrap();
    let (node_a, mut inbox_a) =
        Node::start(&a, LOCALHOST.into(), vec![peer(&b, "b", b_addr.to_string())]).unwrap();

    let from_a = Message::Clip(ClipItem::from_text(a.id, "hello from a"));
    broadcast_when_connected(&node_a, &from_a).await;
    let got = next(&mut inbox_b).await;
    assert_eq!(got.from, a.id);
    assert_eq!(got.message, from_a);

    let from_b = Message::Clip(ClipItem::from_text(b.id, "hello from b"));
    broadcast_when_connected(&node_b, &from_b).await;
    let got = next(&mut inbox_a).await;
    assert_eq!(got.from, b.id);
    assert_eq!(got.message, from_b);

    // Order is preserved on a connection.
    for i in 0..10 {
        node_a.broadcast(&Message::Clip(ClipItem::from_text(a.id, &i.to_string())));
    }
    for i in 0..10 {
        let Message::Clip(item) = next(&mut inbox_b).await.message else {
            panic!("expected a clip");
        };
        assert_eq!(item.text(), Some(i.to_string().as_str()));
    }
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test]
async fn mutual_dialing_settles_on_one_working_connection() {
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();
    let (port_a, port_b) = (free_udp_port(), free_udp_port());

    let (node_a, mut inbox_a) = Node::start(
        &a,
        ([127, 0, 0, 1], port_a).into(),
        vec![peer(&b, "b", format!("127.0.0.1:{port_b}"))],
    )
    .unwrap();
    let (node_b, mut inbox_b) = Node::start(
        &b,
        ([127, 0, 0, 1], port_b).into(),
        vec![peer(&a, "a", format!("127.0.0.1:{port_a}"))],
    )
    .unwrap();

    // Let both dials land and the duplicate get retired.
    broadcast_when_connected(&node_a, &Message::Clip(ClipItem::from_text(a.id, "warmup"))).await;
    next(&mut inbox_b).await;
    sleep(Duration::from_secs(4)).await;

    let from_a = Message::Clip(ClipItem::from_text(a.id, "after dedupe a"));
    let from_b = Message::Clip(ClipItem::from_text(b.id, "after dedupe b"));
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
    let (node_b, _inbox_b) = Node::start(&b, LOCALHOST.into(), vec![peer(&trusted, "t", NOWHERE)]).unwrap();
    let b_addr = node_b.local_addr().unwrap().to_string();
    let (node_s, _inbox_s) = Node::start(&stranger, LOCALHOST.into(), vec![peer(&b, "b", b_addr)]).unwrap();

    sleep(Duration::from_secs(2)).await;
    let msg = Message::Clip(ClipItem::from_text(stranger.id, "let me in"));
    assert_eq!(node_s.broadcast(&msg), 0, "stranger should not be connected");
    assert_eq!(node_b.broadcast(&msg), 0, "b should not have accepted the stranger");
}

#[tokio::test]
async fn impersonating_a_pinned_peer_fails() {
    // The client expects `b`, but `imposter` answers at that address.
    let (a, _a_dir) = identity();
    let (b, _b_dir) = identity();
    let (imposter, _i_dir) = identity();

    let (node_i, _inbox_i) = Node::start(&imposter, LOCALHOST.into(), vec![peer(&a, "a", NOWHERE)]).unwrap();
    let i_addr = node_i.local_addr().unwrap().to_string();
    let (node_a, _inbox_a) = Node::start(&a, LOCALHOST.into(), vec![peer(&b, "b", i_addr)]).unwrap();

    sleep(Duration::from_secs(2)).await;
    let msg = Message::Clip(ClipItem::from_text(a.id, "secret"));
    assert_eq!(node_a.broadcast(&msg), 0, "a must not connect to an imposter");
}
