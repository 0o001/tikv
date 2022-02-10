// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::time::Instant;

use crate::store::{ExecResult, Proposal, RegionSnapshot};
use bytes::Bytes;
use kvproto::kvrpcpb::ExtraOp as TxnExtraOp;
use kvproto::raft_cmdpb::{RaftCmdRequest, RaftCmdResponse};
use kvproto::{metapb, pdpb, raft_serverpb as rspb};
use raft_proto::eraftpb;
use raftstore::store::util::KeysInfoFormatter;

use super::{Peer, RaftApplyState};

#[derive(Debug)]
pub enum PeerMsg {
    RaftMessage(rspb::RaftMessage),
    RaftCommand(RaftCommand),
    Tick,
    Start,
    ApplyResult(MsgApplyResult),
    CasualMessage(CasualMessage),
    /// Message that can't be lost but rarely created. If they are lost, real bad
    /// things happen like some peers will be considered dead in the group.
    SignificantMsg(SignificantMsg),
    GenerateEngineChangeSet(kvenginepb::ChangeSet),
    WaitFollowerSplitFiles(MsgWaitFollowerSplitFiles),
    ApplyChangeSetResult(MsgApplyChangeSetResult),
    CommittedEntries(Vec<eraftpb::Entry>),
}

impl PeerMsg {
    pub(crate) fn take_callback(&mut self) -> Callback {
        match self {
            PeerMsg::RaftCommand(cmd) => std::mem::replace(&mut cmd.callback, Callback::None),
            _ => Callback::None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ApplyMsg {
    Apply(MsgApply),
    Registration(MsgRegistration),
    UnsafeDestroy { region_id: u64 },
    Resume { region_id: u64 },
}

pub enum StoreMsg {
    Tick,
    Start { store: metapb::Store },
    StoreUnreachable { store_id: u64 },
    RaftMessage(rspb::RaftMessage),
    GenerateEngineChangeSet(kvenginepb::ChangeSet),
}

#[derive(Debug)]
pub struct RaftCommand {
    pub send_time: Instant,
    pub request: RaftCmdRequest,
    pub callback: Callback,
}

impl RaftCommand {
    pub fn new(request: RaftCmdRequest, callback: Callback) -> Self {
        Self {
            request,
            callback,
            send_time: Instant::now(),
        }
    }
}

#[derive(Debug)]
pub struct MsgApply {
    pub(crate) region_id: u64,
    pub(crate) term: u64,
    pub(crate) entries: Vec<eraftpb::Entry>,
    pub(crate) new_role: Option<raft::StateRole>,
    pub(crate) cbs: Vec<Proposal>,
}

#[derive(Debug)]
pub struct MsgApplyResult {
    pub(crate) results: VecDeque<ExecResult>,
    pub(crate) apply_state: RaftApplyState,
}

#[derive(Debug)]
pub struct MsgRegistration {
    pub(crate) peer: metapb::Peer,
    pub(crate) term: u64,
    pub(crate) apply_state: RaftApplyState,
    pub(crate) region: metapb::Region,
}

impl MsgRegistration {
    pub(crate) fn new(peer: &Peer) -> Self {
        Self {
            peer: peer.peer.clone(),
            term: peer.term(),
            apply_state: peer.get_store().apply_state(),
            region: peer.get_store().region().clone(),
        }
    }
}

pub struct MsgWaitFollowerSplitFiles {
    pub(crate) split_keys: Vec<Bytes>,
    pub(crate) callback: Callback,
}

impl Debug for MsgWaitFollowerSplitFiles {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "wait follower split files split keys: {:?}",
            self.split_keys,
        )
    }
}

#[derive(Debug)]
pub struct MsgApplyChangeSetResult {
    pub change_set: kvenginepb::ChangeSet,
    pub err: Option<String>,
}

#[derive(Debug)]
pub struct ReadResponse {
    pub response: RaftCmdResponse,
    pub snapshot: Option<RegionSnapshot>,
    pub txn_extra_op: TxnExtraOp,
}

#[derive(Debug)]
pub struct WriteResponse {
    pub response: RaftCmdResponse,
}

pub type ReadCallback = Box<dyn FnOnce(ReadResponse) + Send>;
pub type WriteCallback = Box<dyn FnOnce(WriteResponse) + Send>;
pub type ExtCallback = Box<dyn FnOnce() + Send>;

/// Variants of callbacks for `Msg`.
///  - `Read`: a callback for read only requests including `StatusRequest`,
///         `GetRequest` and `SnapRequest`
///  - `Write`: a callback for write only requests including `AdminRequest`
///          `PutRequest`, `DeleteRequest` and `DeleteRangeRequest`.
pub enum Callback {
    /// No callback.
    None,
    /// Read callback.
    Read(ReadCallback),
    /// Write callback.
    Write {
        cb: WriteCallback,
        /// `proposed_cb` is called after a request is proposed to the raft group successfully.
        /// It's used to notify the caller to move on early because it's very likely the request
        /// will be applied to the raftstore.
        proposed_cb: Option<ExtCallback>,
        /// `committed_cb` is called after a request is committed and before it's being applied, and
        /// it's guaranteed that the request will be successfully applied soon.
        committed_cb: Option<ExtCallback>,
    },
}

impl Callback {
    pub fn write(cb: WriteCallback) -> Self {
        Self::write_ext(cb, None, None)
    }

    pub fn write_ext(
        cb: WriteCallback,
        proposed_cb: Option<ExtCallback>,
        committed_cb: Option<ExtCallback>,
    ) -> Self {
        Callback::Write {
            cb,
            proposed_cb,
            committed_cb,
        }
    }

    pub fn invoke_with_response(self, resp: RaftCmdResponse) {
        match self {
            Callback::None => (),
            Callback::Read(read) => {
                let resp = ReadResponse {
                    response: resp,
                    snapshot: None,
                    txn_extra_op: TxnExtraOp::Noop,
                };
                read(resp);
            }
            Callback::Write { cb, .. } => {
                let resp = WriteResponse { response: resp };
                cb(resp);
            }
        }
    }

    pub fn has_proposed_cb(&mut self) -> bool {
        if let Callback::Write { proposed_cb, .. } = self {
            proposed_cb.is_some()
        } else {
            false
        }
    }

    pub fn invoke_proposed(&mut self) {
        if let Callback::Write { proposed_cb, .. } = self {
            if let Some(cb) = proposed_cb.take() {
                cb()
            }
        }
    }

    pub fn invoke_committed(&mut self) {
        if let Callback::Write { committed_cb, .. } = self {
            if let Some(cb) = committed_cb.take() {
                cb()
            }
        }
    }

    pub fn invoke_read(self, args: ReadResponse) {
        match self {
            Callback::Read(read) => read(args),
            other => panic!("expect Callback::Read(..), got {:?}", other),
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Callback::None)
    }
}

impl fmt::Debug for Callback {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Callback::None => write!(fmt, "Callback::None"),
            Callback::Read(_) => write!(fmt, "Callback::Read(..)"),
            Callback::Write { .. } => write!(fmt, "Callback::Write(..)"),
        }
    }
}

/// Some significant messages sent to raftstore. Raftstore will dispatch these messages to Raft
/// groups to update some important internal status.
#[derive(Debug)]
pub enum SignificantMsg {
    StoreUnreachable {
        store_id: u64,
    },
    /// Reports `to_peer_id` is unreachable.
    Unreachable {
        region_id: u64,
        to_peer_id: u64,
    },
}

/// Message that will be sent to a peer.
///
/// These messages are not significant and can be dropped occasionally.
pub enum CasualMessage {
    /// Split the target region into several partitions.
    SplitRegion {
        region_epoch: metapb::RegionEpoch,
        // It's an encoded key.
        // TODO: support meta key.
        split_keys: Vec<Vec<u8>>,
        callback: Callback,
        source: Cow<'static, str>,
    },

    /// Half split the target region.
    HalfSplitRegion {
        region_epoch: metapb::RegionEpoch,
        policy: pdpb::CheckPolicy,
        source: &'static str,
    },
}

impl fmt::Debug for CasualMessage {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CasualMessage::SplitRegion {
                ref split_keys,
                source,
                ..
            } => write!(
                fmt,
                "Split region with {} from {}",
                KeysInfoFormatter(split_keys.iter()),
                source,
            ),
            CasualMessage::HalfSplitRegion { source, .. } => {
                write!(fmt, "Half Split from {}", source)
            }
        }
    }
}
