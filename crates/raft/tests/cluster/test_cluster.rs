use std::collections::BTreeMap;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::time::Duration;

use maplit::btreemap;
use maplit::btreeset;
use raft::client::ConsensusClient;
use raft::message::Request;
use raft::repr::Node;
use raft::server::start_raft_node;
use tokio::time::sleep;

/// Setup a cluster of 3 nodes.
/// Write to it and read from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_cluster() -> Result<(), Box<dyn std::error::Error>> {
    // --- The client itself does not store addresses for all nodes, but just node id.
    //     Thus we need a supporting component to provide mapping from node id to node address.
    //     This is only used by the client. A raft node in this example stores node addresses in its store.

    // --- Start 3 raft node in 3 threads.
    let d1 = tempdir::TempDir::new("test_cluster")?;
    let d2 = tempdir::TempDir::new("test_cluster")?;
    let d3 = tempdir::TempDir::new("test_cluster")?;

    let _h1 = tokio::spawn(async move {
        let x = start_raft_node(1, d1.path(), get_rpc_addr(1), get_http_addr(1)).await;
        println!("x: {:?}", x);
    });

    let _h2 = tokio::spawn(async move {
        let x = start_raft_node(2, d2.path(), get_rpc_addr(2), get_http_addr(2)).await;
        println!("x: {:?}", x);
    });

    let _h3 = tokio::spawn(async move {
        let x = start_raft_node(3, d3.path(), get_rpc_addr(3), get_http_addr(3)).await;
        println!("x: {:?}", x);
    });

    // Wait for server to start up.
    sleep(Duration::from_millis(500)).await;

    let _h4 = tokio::spawn(async move {
        run_tests().await.expect("test_cluster");
    });

    Ok(())
}

async fn run_tests() -> Result<(), Box<dyn std::error::Error>> {
    // --- Create a client to the first node, as a control handle to the cluster.

    let leader = ConsensusClient::new(1, get_http_addr(1));

    // --- 1. Initialize the target node as a cluster of only one node.
    //        After init(), the single node cluster will be fully functional.

    println!("=== init single node cluster");
    leader.init().await.expect("failed to init cluster");

    println!("=== metrics after init");
    let _x = leader.metrics().await?;

    // --- 2. Add node 2 and 3 to the cluster as `Learner`, to let them start to receive log replication from the
    //        leader.

    println!("=== add-learner 2");
    let _x = leader
        .add_learner((2, get_http_addr(2).to_string(), get_rpc_addr(2).to_string()))
        .await?;

    println!("=== add-learner 3");
    let _x = leader
        .add_learner((3, get_http_addr(2).to_string(), get_rpc_addr(3).to_string()))
        .await?;

    println!("=== metrics after add-learner");
    let x = leader.metrics().await?;

    assert_eq!(&vec![vec![1]], x.membership_config.get_joint_config());

    let nodes_in_cluster = x
        .membership_config
        .nodes()
        .map(|(nid, node)| (*nid, node.clone()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        btreemap! {
            1 => Node{rpc_addr: get_rpc_addr(1).to_string(), api_addr: get_http_addr(1).to_string(), },
            2 => Node{rpc_addr: get_rpc_addr(2).to_string(), api_addr: get_http_addr(2).to_string(), },
            3 => Node{rpc_addr: get_rpc_addr(3).to_string(), api_addr: get_http_addr(3).to_string(), },
        },
        nodes_in_cluster
    );

    // --- 3. Turn the two learners to members. A member node can vote or elect itself as leader.

    println!("=== change-membership to 1,2,3");
    let _x = leader.change_membership(&btreeset! {1,2,3}).await?;

    // --- After change-membership, some cluster state will be seen in the metrics.
    //
    // ```text
    // metrics: RaftMetrics {
    //   current_leader: Some(1),
    //   membership_config: EffectiveMembership {
    //        log_id: LogId { leader_id: LeaderId { term: 1, node_id: 1 }, index: 8 },
    //        membership: Membership { learners: {}, configs: [{1, 2, 3}] }
    //   },
    //   leader_metrics: Some(LeaderMetrics { replication: {
    //     2: ReplicationMetrics { matched: Some(LogId { leader_id: LeaderId { term: 1, node_id: 1 }, index: 7 }) },
    //     3: ReplicationMetrics { matched: Some(LogId { leader_id: LeaderId { term: 1, node_id: 1 }, index: 8 }) }} })
    // }
    // ```

    println!("=== metrics after change-member");
    let x = leader.metrics().await?;
    assert_eq!(&vec![vec![1, 2, 3]], x.membership_config.get_joint_config());

    // --- Try to write some application data through the leader.

    println!("=== write `foo=bar`");
    let _x = leader
        .write(&Request::Set {
            key: "foo".to_string(),
            value: "bar".to_string(),
        })
        .await?;

    // --- Wait for a while to let the replication get done.

    sleep(Duration::from_millis(200)).await;

    // --- Read it on every node.

    println!("=== read `foo` on node 1");
    let x = leader.read(&("foo".to_string())).await?;
    assert_eq!("bar", x);

    println!("=== read `foo` on node 2");
    let client2 = ConsensusClient::new(2, get_http_addr(2));
    let x = client2.read(&("foo".to_string())).await?;
    assert_eq!("bar", x);

    println!("=== read `foo` on node 3");
    let client3 = ConsensusClient::new(3, get_http_addr(3));
    let x = client3.read(&("foo".to_string())).await?;
    assert_eq!("bar", x);

    // --- A write to non-leader will be automatically forwarded to a known leader

    println!("=== read `foo` on node 2");
    let _x = client2
        .write(&Request::Set {
            key: "foo".to_string(),
            value: "wow".to_string(),
        })
        .await?;

    sleep(Duration::from_millis(200)).await;

    // --- Read it on every node.

    println!("=== read `foo` on node 1");
    let x = leader.read(&("foo".to_string())).await?;
    assert_eq!("wow", x);

    println!("=== read `foo` on node 2");
    let client2 = ConsensusClient::new(2, get_http_addr(2));
    let x = client2.read(&("foo".to_string())).await?;
    assert_eq!("wow", x);

    println!("=== read `foo` on node 3");
    let client3 = ConsensusClient::new(3, get_http_addr(3));
    let x = client3.read(&("foo".to_string())).await?;
    assert_eq!("wow", x);

    println!("=== consistent_read `foo` on node 1");
    let x = leader.consistent_read(&("foo".to_string())).await?;
    assert_eq!("wow", x);

    println!("=== consistent_read `foo` on node 2 MUST return CheckIsLeaderError");
    let x = client2.consistent_read(&("foo".to_string())).await;
    match x {
        Err(e) => {
            let s = e.to_string();
            let expect_err:String = "error occur on remote peer 2: has to forward request to: Some(1), Some(ExampleNode { rpc_addr: \"127.0.0.1:22001\", api_addr: \"127.0.0.1:21001\" })".to_string();

            assert_eq!(s, expect_err);
        }
        Ok(_) => panic!("MUST return CheckIsLeaderError"),
    }

    Ok(())
}

fn get_http_addr(node_id: u32) -> SocketAddr {
    let addr = match node_id {
        1 => SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 21001),
        2 => SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 21002),
        3 => SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 21003),
        _ => panic!("node not found"),
    };
    addr
}
fn get_rpc_addr(node_id: u32) -> SocketAddr {
    let addr = match node_id {
        1 => SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 22001),
        2 => SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 22002),
        3 => SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 22003),
        _ => panic!("node not found"),
    };
    addr
}