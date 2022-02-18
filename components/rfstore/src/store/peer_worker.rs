// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use super::*;
use crate::RaftRouter;
use crossbeam::channel::{RecvError, RecvTimeoutError};
use raftstore::store::util;
use std::collections::HashMap;
use std::mem;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tikv_util::mpsc::{Receiver, Sender};
use tikv_util::worker::Scheduler;
use tikv_util::{debug, error, info};

#[derive(Clone)]
pub(crate) struct PeerStates {
    pub(crate) applier: Arc<Mutex<Applier>>,
    pub(crate) peer_fsm: Arc<Mutex<PeerFsm>>,
    pub(crate) closed: Arc<AtomicBool>,
}

impl PeerStates {
    pub(crate) fn new(applier: Applier, peer_fsm: PeerFsm) -> Self {
        Self {
            applier: Arc::new(Mutex::new(applier)),
            peer_fsm: Arc::new(Mutex::new(peer_fsm)),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub(crate) struct PeerInbox {
    pub(crate) peer: PeerStates,
    pub(crate) msgs: Vec<PeerMsg>,
}

pub(crate) struct Inboxes {
    inboxes: HashMap<u64, PeerInbox>,
}

impl Inboxes {
    fn new() -> Self {
        Inboxes {
            inboxes: HashMap::new(),
        }
    }

    fn get_inbox(&mut self, router: &RaftRouter, region_id: u64) -> &mut PeerInbox {
        self.init_inbox(router, region_id);
        self.inboxes.get_mut(&region_id).unwrap()
    }

    fn init_inbox(&mut self, router: &RaftRouter, region_id: u64) {
        if self.inboxes.get_mut(&region_id).is_none() {
            let peer_state = router.peers.get(&region_id).unwrap();
            let inbox = PeerInbox {
                peer: peer_state.clone(),
                msgs: vec![],
            };
            self.inboxes.insert(region_id, inbox);
        }
    }

    fn append_msg(&mut self, router: &RaftRouter, region_id: u64, msg: PeerMsg) {
        self.get_inbox(router, region_id).msgs.push(msg)
    }
}

pub(crate) struct RaftWorker {
    ctx: RaftContext,
    receiver: Receiver<(u64, PeerMsg)>,
    router: RaftRouter,
    apply_senders: Vec<Sender<ApplyBatch>>,
    io_sender: Sender<IOTask>,
    last_tick: Instant,
    tick_millis: u64,
}

impl RaftWorker {
    pub(crate) fn new(
        ctx: GlobalContext,
        receiver: Receiver<(u64, PeerMsg)>,
        router: RaftRouter,
        io_sender: Sender<IOTask>,
    ) -> (Self, Vec<Receiver<ApplyBatch>>) {
        let apply_pool_size = ctx.cfg.value().apply_pool_size;
        let mut apply_senders = Vec::with_capacity(apply_pool_size);
        let mut apply_receivers = Vec::with_capacity(apply_pool_size);
        for _ in 0..apply_pool_size {
            let (sender, receiver) = tikv_util::mpsc::unbounded();
            apply_senders.push(sender);
            apply_receivers.push(receiver);
        }
        let tick_millis = ctx.cfg.value().raft_base_tick_interval.as_millis();
        let ctx = RaftContext::new(ctx);
        (
            Self {
                ctx,
                receiver,
                router,
                apply_senders,
                io_sender,
                last_tick: Instant::now(),
                tick_millis,
            },
            apply_receivers,
        )
    }

    pub(crate) fn run(&mut self) {
        let mut inboxes = Inboxes::new();
        loop {
            if self.receive_msgs(&mut inboxes).is_err() {
                return;
            }
            inboxes.inboxes.iter_mut().for_each(|(_, inbox)| {
                self.process_inbox(inbox);
            });
            if self.ctx.global.trans.need_flush() {
                self.ctx.global.trans.flush();
            }
            self.persist_state();
            self.ctx.current_time = None;
        }
    }

    /// return true means channel is disconnected, return outer loop.
    fn receive_msgs(&mut self, inboxes: &mut Inboxes) -> std::result::Result<(), RecvTimeoutError> {
        inboxes.inboxes.retain(|_, inbox| -> bool {
            if inbox.msgs.len() == 0 {
                false
            } else {
                inbox.msgs.truncate(0);
                true
            }
        });
        let res = self.receiver.recv_timeout(Duration::from_millis(10));
        let router = &self.router;
        match res {
            Ok((region_id, msg)) => {
                inboxes.append_msg(router, region_id, msg);
                loop {
                    if let Ok((region_id, msg)) = self.receiver.try_recv() {
                        inboxes.append_msg(router, region_id, msg);
                    } else {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if (now - self.last_tick).as_millis() as u64 > self.tick_millis {
            self.last_tick = now;
            let peers = self.router.peers.clone();
            for x in peers.iter() {
                let region_id = *x.key();
                inboxes.append_msg(router, region_id, PeerMsg::Tick);
            }
        }
        return Ok(());
    }

    fn process_inbox(&mut self, inbox: &mut PeerInbox) {
        if inbox.msgs.is_empty() {
            return;
        }
        let mut peer_fsm = inbox.peer.peer_fsm.lock().unwrap();
        if peer_fsm.stopped {
            return;
        }
        PeerMsgHandler::new(&mut peer_fsm, &mut self.ctx).handle_msgs(&mut inbox.msgs);
        peer_fsm.peer.handle_raft_ready(&mut self.ctx);
        self.maybe_send_apply(&inbox.peer.applier, &peer_fsm);
        peer_fsm.peer.maybe_finish_split(&mut self.ctx);
    }

    fn maybe_send_apply(&mut self, applier: &Arc<Mutex<Applier>>, peer_fsm: &PeerFsm) {
        if !self.ctx.apply_msgs.msgs.is_empty() {
            let peer_batch = ApplyBatch {
                msgs: mem::take(&mut self.ctx.apply_msgs.msgs),
                applier: applier.clone(),
                applying_cnt: peer_fsm.applying_cnt.clone(),
            };
            peer_batch
                .applying_cnt
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.apply_senders[peer_fsm.apply_worker_idx]
                .send(peer_batch)
                .unwrap();
        }
    }

    fn persist_state(&mut self) {
        if self.ctx.persist_readies.is_empty() && self.ctx.raft_wb.is_empty() {
            return;
        }
        let raft_wb = mem::take(&mut self.ctx.raft_wb);
        self.ctx.global.engines.raft.apply(&raft_wb);
        let readies = mem::take(&mut self.ctx.persist_readies);
        let io_task = IOTask { raft_wb, readies };
        self.io_sender.send(io_task).unwrap();
    }
}

pub(crate) struct ApplyWorker {
    ctx: ApplyContext,
    receiver: Receiver<ApplyBatch>,
}

impl ApplyWorker {
    pub(crate) fn new(
        engine: kvengine::Engine,
        region_sched: Scheduler<RegionTask>,
        split_scheduler: Scheduler<SplitTask>,
        router: RaftRouter,
        receiver: Receiver<ApplyBatch>,
    ) -> Self {
        let ctx = ApplyContext::new(
            engine,
            Some(region_sched),
            Some(split_scheduler),
            Some(router),
        );
        Self { ctx, receiver }
    }

    pub(crate) fn run(&mut self) {
        loop {
            let res = self.receiver.recv();
            if res.is_err() {
                return;
            }
            let mut batch = res.unwrap();
            let mut applier = batch.applier.lock().unwrap();
            for msg in batch.msgs.drain(..) {
                applier.handle_msg(&mut self.ctx, msg);
            }
            batch
                .applying_cnt
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

pub(crate) struct StoreWorker {
    handler: StoreMsgHandler,
    last_tick: Instant,
    tick_millis: u64,
}

impl StoreWorker {
    pub(crate) fn new(store_fsm: StoreFSM, ctx: GlobalContext) -> Self {
        let tick_millis = ctx.cfg.value().raft_base_tick_interval.as_millis();
        let handler = StoreMsgHandler::new(store_fsm, ctx);
        Self {
            handler,
            last_tick: Instant::now(),
            tick_millis,
        }
    }

    pub(crate) fn run(&mut self) {
        loop {
            let res = self
                .handler
                .get_receiver()
                .recv_timeout(Duration::from_millis(10));
            match res {
                Ok(msg) => self.handler.handle_msg(msg),
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            if (now - self.last_tick).as_millis() as u64 > self.tick_millis {
                self.handler.handle_msg(StoreMsg::Tick);
                self.last_tick = now;
            }
        }
    }
}

pub(crate) struct IOWorker {
    engine: rfengine::RFEngine,
    receiver: Receiver<IOTask>,
    router: RaftRouter,
    trans: Box<dyn Transport>,
}

impl IOWorker {
    pub(crate) fn new(
        engine: rfengine::RFEngine,
        router: RaftRouter,
        trans: Box<dyn Transport>,
    ) -> (Self, Sender<IOTask>) {
        let (sender, receiver) = tikv_util::mpsc::bounded(0);
        (
            Self {
                engine,
                receiver,
                router,
                trans,
            },
            sender,
        )
    }

    pub(crate) fn run(&mut self) {
        loop {
            let res = self.receiver.recv_timeout(Duration::from_secs(1));
            match res {
                Ok(msg) => self.handle_msg(msg),
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn handle_msg(&mut self, task: IOTask) {
        if !task.raft_wb.is_empty() {
            self.engine.persist(task.raft_wb).unwrap();
        }
        for mut ready in task.readies {
            let raft_messages = mem::take(&mut ready.raft_messages);
            for msg in raft_messages {
                debug!(
                    "follower send raft message";
                    "region_id" => msg.region_id,
                    "message_type" => %util::MsgType(&msg),
                    "from_peer_id" => msg.get_from_peer().get_id(),
                    "to_peer_id" => msg.get_to_peer().get_id(),
                );
                if let Err(err) = self.trans.send(msg) {
                    error!("failed to send persist raft message {:?}", err);
                }
            }
            let region_id = ready.region_id;
            let msg = PeerMsg::Persisted(ready);
            if let Err(err) = self.router.send(region_id, msg) {
                error!("failed to send persisted message {:?}", err);
            }
        }
        if self.trans.need_flush() {
            self.trans.flush();
        }
    }
}
