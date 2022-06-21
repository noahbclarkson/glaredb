pub mod coordinator;
pub mod replica;

use super::keys::Key;
use super::log::Log;
use super::protocol::{ApplyOk, Message, ProtocolMessage, ReadOk, StartExecuteInternal};
use super::topology::{Address, TopologyManagerRef};
use super::{AccordError, Executor, NodeId, Result};
use coordinator::{AcceptOrCommit, CoordinatorState};
use log::{debug, error, info};
use replica::{ExecutionActionOk, ReplicaState};
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct Node {
    node: NodeId,
}

impl Node {
    pub async fn start<K, E>(
        log: Log,
        node: NodeId,
        tm: TopologyManagerRef,
        executor: E,
        inbound: mpsc::UnboundedReceiver<Message<K>>,
        outbound: mpsc::UnboundedSender<Message<K>>,
    ) -> Result<Node>
    where
        K: Key,
        E: Executor<K>,
    {
        let replica = ReplicaState::<K>::new(log, node);
        let coordinator = CoordinatorState::<K>::new(tm, node);

        let driver = StateDriver::new(coordinator, replica, inbound, outbound);

        tokio::spawn(async {
            info!("starting state driver");
            // TODO: Handle error better.
            // TODO: Restart on error? Use a oneshot to indicate state error?
            match driver.start(executor).await {
                Ok(_) => info!("shutting down"),
                Err(e) => error!("received error from state driver: {}", e),
            }
        });

        Ok(Node { node })
    }
}

struct StateDriver<K> {
    inbound: mpsc::UnboundedReceiver<Message<K>>,
    outbound: mpsc::UnboundedSender<Message<K>>,
    replica: ReplicaState<K>,
    coordinator: CoordinatorState<K>,
}

impl<K: Key> StateDriver<K> {
    fn new(
        coordinator: CoordinatorState<K>,
        replica: ReplicaState<K>,
        inbound: mpsc::UnboundedReceiver<Message<K>>,
        outbound: mpsc::UnboundedSender<Message<K>>,
    ) -> Self {
        StateDriver {
            inbound,
            outbound,
            replica,
            coordinator,
        }
    }

    async fn start<E>(mut self, executor: E) -> Result<()>
    where
        E: Executor<K>,
    {
        info!("starting state driver");
        while let Some(msg) = self.inbound.recv().await {
            debug!("received message: {:?}", msg);
            self.handle_msg(&executor, msg).await?;
        }
        Ok(())
    }

    async fn handle_msg<E>(&mut self, executor: &E, msg: Message<K>) -> Result<()>
    where
        E: Executor<K>,
    {
        use ProtocolMessage::*;

        let from = msg.from;
        match msg.proto_msg {
            BeginRead { keys, command } => {
                let msg = self.coordinator.new_read_tx(keys, command);
                self.send_outbound(Address::Peers, ProtocolMessage::PreAccept(msg))?;
            }
            BeginWrite { keys, command } => {
                let msg = self.coordinator.new_write_tx(keys, command);
                self.send_outbound(Address::Peers, ProtocolMessage::PreAccept(msg))?;
            }
            StartExecute(msg) => {
                let msg = self.coordinator.start_execute(msg)?;
                self.send_outbound(Address::Peers, ProtocolMessage::Read(msg))?;
            }
            PreAccept(msg) => {
                let msg = self.replica.receive_preaccept(msg);
                self.send_outbound(Address::Peer(from), ProtocolMessage::PreAcceptOk(msg))?;
            }
            PreAcceptOk(msg) => {
                let msg = self.coordinator.store_proposal(from, msg)?;
                match msg {
                    Some(AcceptOrCommit::Accept(msg)) => {
                        self.send_outbound(Address::Peers, ProtocolMessage::Accept(msg))?;
                    }
                    Some(AcceptOrCommit::Commit(msg)) => {
                        let id = msg.tx.get_id().clone();
                        self.send_outbound(Address::Peers, ProtocolMessage::Commit(msg))?;
                        self.send_outbound(
                            Address::Local,
                            ProtocolMessage::StartExecute(StartExecuteInternal { tx: id }),
                        )?;
                    }
                    _ => (), // Nothing to send yet.
                }
            }
            Accept(msg) => {
                let msg = self.replica.receive_accept(msg);
                self.send_outbound(Address::Peer(from), ProtocolMessage::AcceptOk(msg))?;
            }
            AcceptOk(msg) => {
                let msg = self.coordinator.store_accept_ok(from, msg)?;
                if let Some(msg) = msg {
                    let id = msg.tx.get_id().clone();
                    self.send_outbound(Address::Peers, ProtocolMessage::Commit(msg))?;
                    self.send_outbound(
                        Address::Local,
                        ProtocolMessage::StartExecute(StartExecuteInternal { tx: id }),
                    )?;
                }
            }
            Commit(msg) => {
                self.replica.receive_commit(msg)?;
            }
            Read(msg) => {
                let actions = self.replica.receive_read(executor, msg)?;
                self.send_execution_actions(Address::Peer(from), actions)?;
            }
            ReadOk(msg) => {
                let msg = self.coordinator.store_read_ok(msg)?;
                if let Some(msg) = msg {
                    self.send_outbound(Address::Peers, ProtocolMessage::Apply(msg))?;
                }
            }
            Apply(msg) => {
                let actions = self.replica.receive_apply(executor, msg)?;
                self.send_execution_actions(Address::Peer(from), actions)?;
            }
            ApplyOk(msg) => {
                // TODO: Handle
                info!("received apply ok: {:?}", msg);
            }
        };

        Ok(())
    }

    fn send_outbound(&self, to: Address, msg: ProtocolMessage<K>) -> Result<()> {
        let msg = Message {
            from: self.replica.get_node_id(),
            to,
            proto_msg: msg,
        };
        match self.outbound.send(msg) {
            Ok(_) => Ok(()),
            Err(msg) => Err(AccordError::OutboundSend(format!("msg: {:?}", msg))),
        }
    }

    fn send_execution_actions(&self, to: Address, actions: Vec<ExecutionActionOk>) -> Result<()> {
        for action in actions.into_iter() {
            match action {
                ExecutionActionOk::ReadOk(msg) => {
                    self.send_outbound(to.clone(), ProtocolMessage::ReadOk(msg))?;
                }
                ExecutionActionOk::ApplyOk(msg) => {
                    self.send_outbound(to.clone(), ProtocolMessage::ApplyOk(msg))?;
                }
            }
        }
        Ok(())
    }
}
