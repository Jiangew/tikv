// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::{Arc, RwLock, Mutex};
use std::sync::mpsc::Sender;
use std::net::SocketAddr;
use kvproto::raft_serverpb::RaftMessage;
use kvproto::raft_cmdpb::RaftCmdRequest;

use util::transport::SendCh;
use util::HandyRwLock;
use util::worker::{Stopped, Scheduler};
use util::collections::HashSet;
use raft::SnapshotStatus;
use raftstore::store::{Msg as StoreMsg, SnapshotStatusMsg, Transport, Callback};
use raftstore::Result as RaftStoreResult;
use server::raft_client::RaftClient;
use server::Result;
use super::snap::Task as SnapTask;
use super::resolve::StoreAddrResolver;
use super::metrics::*;

pub trait RaftStoreRouter: Send + Clone {
    /// Send StoreMsg, retry if failed. Try times may vary from implementation.
    fn send(&self, msg: StoreMsg) -> RaftStoreResult<()>;

    /// Send StoreMsg.
    fn try_send(&self, msg: StoreMsg) -> RaftStoreResult<()>;

    // Send RaftMessage to local store.
    fn send_raft_msg(&self, msg: RaftMessage) -> RaftStoreResult<()> {
        self.try_send(StoreMsg::RaftMessage(msg))
    }

    // Send RaftCmdRequest to local store.
    fn send_command(&self, req: RaftCmdRequest, cb: Callback) -> RaftStoreResult<()> {
        self.try_send(StoreMsg::new_raft_cmd(req, cb))
    }

    fn report_unreachable(&self, region_id: u64, to_peer_id: u64, _: u64) -> RaftStoreResult<()> {
        self.try_send(StoreMsg::ReportUnreachable {
            region_id: region_id,
            to_peer_id: to_peer_id,
        })
    }
}

#[derive(Clone)]
pub struct ServerRaftStoreRouter {
    pub ch: SendCh<StoreMsg>,
}

impl ServerRaftStoreRouter {
    pub fn new(ch: SendCh<StoreMsg>) -> ServerRaftStoreRouter {
        ServerRaftStoreRouter { ch: ch }
    }

    //    fn validate_store_id(&self, store_id: u64) -> RaftStoreResult<()> {
    //        if store_id != self.store_id {
    //            let store = store_id.to_string();
    //            REPORT_FAILURE_MSG_COUNTER.with_label_values(&["store_not_match", &*store]).inc();
    //            Err(RaftStoreError::StoreNotMatch(store_id, self.store_id))
    //        } else {
    //            Ok(())
    //        }
    //    }
}

impl RaftStoreRouter for ServerRaftStoreRouter {
    fn try_send(&self, msg: StoreMsg) -> RaftStoreResult<()> {
        try!(self.ch.try_send(msg));
        Ok(())
    }

    fn send(&self, msg: StoreMsg) -> RaftStoreResult<()> {
        try!(self.ch.send(msg));
        Ok(())
    }

    fn send_raft_msg(&self, msg: RaftMessage) -> RaftStoreResult<()> {
        //        let store_id = msg.get_to_peer().get_store_id();
        //        try!(self.validate_store_id(store_id));
        self.try_send(StoreMsg::RaftMessage(msg))
    }

    fn send_command(&self, req: RaftCmdRequest, cb: Callback) -> RaftStoreResult<()> {
        //        let store_id = req.get_header().get_peer().get_store_id();
        //        try!(self.validate_store_id(store_id));
        self.try_send(StoreMsg::new_raft_cmd(req, cb))
    }

    fn report_unreachable(&self,
                          region_id: u64,
                          to_peer_id: u64,
                          to_store_id: u64)
                          -> RaftStoreResult<()> {
        let store = to_store_id.to_string();
        REPORT_FAILURE_MSG_COUNTER.with_label_values(&["unreachable", &*store]).inc();
        self.try_send(StoreMsg::ReportUnreachable {
            region_id: region_id,
            to_peer_id: to_peer_id,
        })
    }
}

pub struct ServerTransport<T, S>
    where T: RaftStoreRouter + 'static,
          S: StoreAddrResolver + Send + 'static
{
    raft_client: Arc<RwLock<RaftClient>>,
    snap_scheduler: Scheduler<SnapTask>,
    raft_router: T,
    snapshot_status_sender: Sender<SnapshotStatusMsg>,
    resolving: Arc<RwLock<HashSet<u64>>>,
    resolver: Arc<Mutex<S>>,
}

impl<T, S> Clone for ServerTransport<T, S>
    where T: RaftStoreRouter + 'static,
          S: StoreAddrResolver + Send + 'static
{
    fn clone(&self) -> Self {
        ServerTransport {
            raft_client: self.raft_client.clone(),
            snap_scheduler: self.snap_scheduler.clone(),
            raft_router: self.raft_router.clone(),
            snapshot_status_sender: self.snapshot_status_sender.clone(),
            resolving: self.resolving.clone(),
            resolver: self.resolver.clone(),
        }
    }
}

impl<T: RaftStoreRouter + 'static, S: StoreAddrResolver + Send + 'static> ServerTransport<T, S> {
    pub fn new(raft_client: Arc<RwLock<RaftClient>>,
               snap_scheduler: Scheduler<SnapTask>,
               raft_router: T,
               snapshot_status_sender: Sender<SnapshotStatusMsg>,
               resolver: S)
               -> ServerTransport<T, S> {
        ServerTransport {
            raft_client: raft_client,
            snap_scheduler: snap_scheduler,
            raft_router: raft_router,
            snapshot_status_sender: snapshot_status_sender,
            resolving: Arc::new(RwLock::new(Default::default())),
            resolver: Arc::new(Mutex::new(resolver)),
        }
    }

    fn send_store(&self, store_id: u64, msg: RaftMessage) {
        // check the corresponding token for store.
        let addr = self.raft_client.rl().addrs.get(&store_id).map(|x| x.to_owned());
        if let Some(addr) = addr {
            self.write_data(store_id, addr, msg);
            return;
        }

        // No connection, try to resolve it.
        if self.resolving.rl().contains(&store_id) {
            RESOLVE_STORE_COUNTER.with_label_values(&["resolving"]).inc();
            // If we are resolving the address, drop the message here.
            debug!("store {} address is being resolved, drop msg {:?}",
                   store_id,
                   msg);
            self.report_unreachable(msg);
            return;
        }

        debug!("begin to resolve store {} address", store_id);
        let label = if msg.get_message().has_snapshot() {
            "snap"
        } else {
            "store"
        };
        RESOLVE_STORE_COUNTER.with_label_values(&[label]).inc();

        self.resolving.wl().insert(store_id);
        self.resolve(store_id, msg);
    }

    fn resolve(&self, store_id: u64, msg: RaftMessage) {
        let trans = self.clone();
        let cb = box move |addr| {
            // clear resolving.
            trans.resolving.wl().remove(&store_id);

            if let Err(e) = addr {
                RESOLVE_STORE_COUNTER.with_label_values(&["failed"]).inc();
                debug!("resolve store {} address failed {:?}", store_id, e);
                trans.report_unreachable(msg);
                return;
            }

            RESOLVE_STORE_COUNTER.with_label_values(&["success"]).inc();
            let addr = addr.unwrap();
            info!("resolve store {} address ok, addr {}", store_id, addr);
            trans.raft_client.wl().addrs.insert(store_id, addr);
            trans.write_data(store_id, addr, msg);
        };
        if let Err(e) = self.resolver.lock().unwrap().resolve(store_id, cb) {
            error!("try to resolve err {:?}", e);
        }
    }

    fn write_data(&self, store_id: u64, addr: SocketAddr, msg: RaftMessage) {
        if msg.get_message().has_snapshot() {
            return self.send_snapshot_sock(addr, msg);
        }

        if let Err(e) = self.raft_client.wl().send(store_id, addr, msg) {
            error!("send raft msg err {:?}", e);
        }
    }

    fn send_snapshot_sock(&self, sock_addr: SocketAddr, msg: RaftMessage) {
        let rep = self.new_snapshot_reporter(&msg);
        let cb = box move |res: Result<()>| {
            if res.is_err() {
                rep.report(SnapshotStatus::Failure);
            } else {
                rep.report(SnapshotStatus::Finish);
            }
        };
        if let Err(Stopped(SnapTask::SendTo { cb, .. })) = self.snap_scheduler
            .schedule(SnapTask::SendTo {
                addr: sock_addr,
                msg: msg,
                cb: cb,
            }) {
            error!("channel is closed, failed to schedule snapshot to {}",
                   sock_addr);
            cb(Err(box_err!("failed to schedule snapshot")));
        }
    }

    fn new_snapshot_reporter(&self, msg: &RaftMessage) -> SnapshotReporter {
        let region_id = msg.get_region_id();
        let to_peer_id = msg.get_to_peer().get_id();
        let to_store_id = msg.get_to_peer().get_store_id();

        SnapshotReporter {
            snapshot_status_sender: self.snapshot_status_sender.clone(),
            region_id: region_id,
            to_peer_id: to_peer_id,
            to_store_id: to_store_id,
        }
    }

    pub fn report_unreachable(&self, msg: RaftMessage) {
        let region_id = msg.get_region_id();
        let to_peer_id = msg.get_to_peer().get_id();
        let to_store_id = msg.get_to_peer().get_store_id();

        if let Err(e) = self.raft_router.report_unreachable(region_id, to_peer_id, to_store_id) {
            error!("report peer {} unreachable for region {} failed {:?}",
                   to_peer_id,
                   region_id,
                   e);
        }
    }
}

impl<T, S> Transport for ServerTransport<T, S>
    where T: RaftStoreRouter + 'static,
          S: StoreAddrResolver + Send + 'static
{
    fn send(&self, msg: RaftMessage) -> RaftStoreResult<()> {
        let to_store_id = msg.get_to_peer().get_store_id();
        self.send_store(to_store_id, msg);
        Ok(())
    }
}


struct SnapshotReporter {
    snapshot_status_sender: Sender<SnapshotStatusMsg>,
    region_id: u64,
    to_peer_id: u64,
    to_store_id: u64,
}

impl SnapshotReporter {
    pub fn report(&self, status: SnapshotStatus) {
        debug!("send snapshot to {} for {} {:?}",
               self.to_peer_id,
               self.region_id,
               status);

        if status == SnapshotStatus::Failure {
            let store = self.to_store_id.to_string();
            REPORT_FAILURE_MSG_COUNTER.with_label_values(&["snapshot", &*store]).inc();
        };

        if let Err(e) = self.snapshot_status_sender.send(SnapshotStatusMsg {
            region_id: self.region_id,
            to_peer_id: self.to_peer_id,
            status: status,
        }) {
            error!("report snapshot to peer {} in store {} with region {} err {:?}",
                   self.to_peer_id,
                   self.to_store_id,
                   self.region_id,
                   e);
        }
    }
}

// MockRaftStoreRouter is used for passing compile.
#[derive(Clone)]
pub struct MockRaftStoreRouter;

impl RaftStoreRouter for MockRaftStoreRouter {
    fn send(&self, _: StoreMsg) -> RaftStoreResult<()> {
        unimplemented!();
    }

    fn try_send(&self, _: StoreMsg) -> RaftStoreResult<()> {
        unimplemented!();
    }
}

#[cfg(test)]
mod tests {
    extern crate mio;

    use super::*;
    use raftstore::store::Msg;
    use util::transport::SendCh;
    use kvproto::metapb::Peer;
    use kvproto::raft_serverpb::RaftMessage;
    use kvproto::raft_cmdpb::{RaftRequestHeader, RaftCmdRequest};
    use mio::{EventLoop, Handler};

    struct FooHandler;

    impl Handler for FooHandler {
        type Timeout = ();
        type Message = Msg;
    }

    fn new_raft_msg(store_id: u64) -> RaftMessage {
        let mut peer = Peer::new();
        peer.set_store_id(store_id);
        let mut msg = RaftMessage::new();
        msg.set_to_peer(peer);
        msg
    }

    fn new_raft_cmd(store_id: u64) -> RaftCmdRequest {
        let mut peer = Peer::new();
        peer.set_store_id(store_id);
        let mut header = RaftRequestHeader::new();
        header.set_peer(peer);
        let mut msg = RaftCmdRequest::new();
        msg.set_header(header);
        msg
    }

    // #[test]
    fn test_store_not_match() {
        let store_id = 1;
        let invalid_store_id = store_id + 1;

        let evloop = EventLoop::<FooHandler>::new().unwrap();
        let sendch = SendCh::new(evloop.channel(), "test-store");
        let router = ServerRaftStoreRouter::new(sendch);

        let msg = new_raft_msg(store_id);
        let cmd = new_raft_cmd(store_id);
        assert!(router.send_raft_msg(msg).is_ok());
        let cb = |_| {};
        assert!(router.send_command(cmd, box cb).is_ok());

        let msg = new_raft_msg(invalid_store_id);
        let cmd = new_raft_cmd(invalid_store_id);
        assert!(router.send_raft_msg(msg).is_err());
        let cb = |_| {};
        assert!(router.send_command(cmd, box cb).is_err());
    }
}
