use std::{collections::BTreeSet, net::SocketAddr, sync::Arc};

use openraft::error::{NetworkError, RemoteError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::trace;

use super::{error::RpcResult, message::Request};
use crate::error::Result;
use crate::{
    error::RpcError,
    openraft_types::types::{
        AddLearnerError, AddLearnerResponse, CheckIsLeaderError, ClientWriteError,
        ClientWriteResponse, ForwardToLeader, Infallible, InitializeError, RaftMetrics,
    },
    repr::NodeId,
};

pub mod rpc;

pub struct ConsensusClient {
    pub leader: Arc<Mutex<(NodeId, SocketAddr)>>,
    pub inner: reqwest::Client,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Empty;

impl ConsensusClient {
    /// Create a client with a leader node id and a node manager to get node address by node id.
    pub fn new(leader_id: NodeId, leader_addr: SocketAddr) -> Self {
        Self {
            leader: Arc::new(Mutex::new((leader_id, leader_addr))),
            inner: reqwest::Client::new(),
        }
    }

    // --- Application API

    /// Submit a write request to the raft cluster.
    ///
    /// The request will be processed by raft protocol: it will be replicated to a quorum and then will be applied to
    /// state machine.
    ///
    /// The result of applying the request will be returned.
    pub async fn write(&self, req: &Request) -> RpcResult<ClientWriteResponse, ClientWriteError> {
        self.send_rpc_to_leader("api/write", Some(req)).await
    }

    /// Read value by key, in an inconsistent mode.
    ///
    /// This method may return stale value because it does not force to read on a legal leader.
    pub async fn read(&self, req: &String) -> RpcResult<String, Infallible> {
        self.do_send_rpc_to_leader("api/read", Some(req)).await
    }

    /// Consistent Read value by key, in an inconsistent mode.
    ///
    /// This method MUST return consitent value or CheckIsLeaderError.
    pub async fn consistent_read(&self, req: &String) -> RpcResult<String, CheckIsLeaderError> {
        self.do_send_rpc_to_leader("api/consistent_read", Some(req))
            .await
    }

    // --- Cluster management API

    /// Initialize a cluster of only the node that receives this request.
    ///
    /// This is the first step to initialize a cluster.
    /// With a initialized cluster, new node can be added with [`write`].
    /// Then setup replication with [`add_learner`].
    /// Then make the new node a member with [`change_membership`].
    pub async fn init(&self) -> RpcResult<(), InitializeError> {
        self.do_send_rpc_to_leader("cluster/init", Some(&Empty {}))
            .await
    }

    /// Add a node as learner.
    ///
    /// The node to add has to exist, i.e., being added with `write(Request::AddNode{})`
    pub async fn add_learner(
        &self,
        req: (NodeId, String, String),
    ) -> RpcResult<AddLearnerResponse, AddLearnerError> {
        self.send_rpc_to_leader("cluster/add-learner", Some(&req))
            .await
    }

    /// Change membership to the specified set of nodes.
    ///
    /// All nodes in `req` have to be already added as learner with [`add_learner`],
    /// or an error [`LearnerNotFound`] will be returned.
    pub async fn change_membership(
        &self,
        req: &BTreeSet<NodeId>,
    ) -> RpcResult<ClientWriteResponse, ClientWriteError> {
        self.send_rpc_to_leader("cluster/change-membership", Some(req))
            .await
    }

    /// Get the metrics about the cluster.
    ///
    /// Metrics contains various information about the cluster, such as current leader,
    /// membership config, replication status etc.
    /// See [`RaftMetrics`].
    pub async fn metrics(&self) -> RpcResult<RaftMetrics, Infallible> {
        self.do_send_rpc_to_leader("cluster/metrics", None::<&()>)
            .await
    }

    // --- Internal methods

    /// Send RPC to specified node.
    ///
    /// It sends out a POST request if `req` is Some. Otherwise a GET request.
    /// The remote endpoint must respond a reply in form of `Result<T, E>`.
    /// An `Err` happened on remote will be wrapped in an [`RPCError::RemoteError`].
    async fn do_send_rpc_to_leader<Req, Resp, Err>(
        &self,
        uri: &str,
        req: Option<&Req>,
    ) -> RpcResult<Resp, Err>
    where
        Req: Serialize + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned,
    {
        let (leader_id, url) = {
            let t = self.leader.lock().await;
            let target_addr = &t.1;
            (t.0, format!("http://{}/{}", target_addr, uri))
        };

        let resp = if let Some(r) = req {
            trace!(
                ">>> client send request to {}: {}",
                url,
                serde_json::to_string_pretty(&r).unwrap()
            );
            self.inner.post(url.clone()).json(r)
        } else {
            trace!(">>> client send request to {}", url,);
            self.inner.get(url.clone())
        }
        .send()
        .await
        .map_err(|e| RpcError::Network(NetworkError::new(&e)))?;

        let res: Result<Resp, Err> = resp
            .json()
            .await
            .map_err(|e| RpcError::Network(NetworkError::new(&e)))?;
        trace!(
            "<<< client recv reply from {}: {}",
            url,
            serde_json::to_string_pretty(&res).unwrap()
        );

        res.map_err(|e| RpcError::RemoteError(RemoteError::new(leader_id, e)))
    }

    /// Try the best to send a request to the leader.
    ///
    /// If the target node is not a leader, a `ForwardToLeader` error will be
    /// returned and this client will retry at most 3 times to contact the updated leader.
    async fn send_rpc_to_leader<Req, Resp, Err>(
        &self,
        uri: &str,
        req: Option<&Req>,
    ) -> RpcResult<Resp, Err>
    where
        Req: Serialize + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned + TryInto<ForwardToLeader> + Clone,
    {
        // Retry at most 3 times to find a valid leader.
        let mut n_retry = 3;

        loop {
            let res: RpcResult<Resp, Err> = self.do_send_rpc_to_leader(uri, req).await;

            let rpc_err = match res {
                Ok(x) => return Ok(x),
                Err(rpc_err) => rpc_err,
            };

            if let RpcError::RemoteError(remote_err) = &rpc_err {
                let forward_err_res =
                    <Err as TryInto<ForwardToLeader>>::try_into(remote_err.source.clone());

                if let Ok(ForwardToLeader {
                    leader_id: Some(leader_id),
                    leader_node: Some(leader_node),
                    ..
                }) = forward_err_res
                {
                    // Update target to the new leader.
                    {
                        let mut t = self.leader.lock().await;
                        let api_addr = leader_node.api_addr.clone();
                        *t = (leader_id, api_addr.parse::<SocketAddr>().unwrap());
                    }

                    n_retry -= 1;
                    if n_retry > 0 {
                        continue;
                    }
                }
            }

            return Err(rpc_err);
        }
    }
}