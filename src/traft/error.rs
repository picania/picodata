use std::fmt::{Debug, Display};

use crate::instance::InstanceId;
use crate::traft::{RaftId, RaftTerm};
use tarantool::error::{BoxError, IntoBoxError};
use tarantool::fiber::r#async::timeout;
use tarantool::tlua::LuaError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("uninitialized yet")]
    Uninitialized,
    #[error("timeout")]
    Timeout,
    #[error("current instance is expelled from the cluster")]
    Expelled,
    #[error("{0}")]
    Raft(#[from] raft::Error),
    #[error("downcast error: expected {expected:?}, actual: {actual:?}")]
    DowncastError {
        expected: &'static str,
        actual: &'static str,
    },
    /// cluster_id of the joining instance mismatches the cluster_id of the cluster
    #[error("cluster_id mismatch: cluster_id of the instance = {instance_cluster_id:?}, cluster_id of the cluster = {cluster_cluster_id:?}")]
    ClusterIdMismatch {
        instance_cluster_id: String,
        cluster_cluster_id: String,
    },
    /// Instance was requested to configure replication with different replicaset.
    #[error("cannot replicate with different replicaset: expected {instance_rsid:?}, requested {requested_rsid:?}")]
    ReplicasetIdMismatch {
        instance_rsid: String,
        requested_rsid: String,
    },
    // NOTE: this error message is relied on in luamod.lua,
    // don't forget to update it everywhere if you're changing it.
    #[error("operation request from different term {requested}, current term is {current}")]
    TermMismatch {
        requested: RaftTerm,
        current: RaftTerm,
    },
    // NOTE: this error message is relied on in luamod.lua,
    // don't forget to update it everywhere if you're changing it.
    #[error("not a leader")]
    NotALeader,
    #[error("lua error: {0}")]
    Lua(#[from] LuaError),
    #[error("{0}")]
    Tarantool(#[from] ::tarantool::error::Error),
    #[error("instance with id {0} not found")]
    NoInstanceWithRaftId(RaftId),
    #[error("instance with id \"{0}\" not found")]
    NoInstanceWithInstanceId(InstanceId),
    #[error("address of peer with id {0} not found")]
    AddressUnknownForRaftId(RaftId),
    #[error("address of peer with id \"{0}\" not found")]
    AddressUnknownForInstanceId(InstanceId),
    #[error("address of peer is incorrectly formatted: {0}")]
    AddressParseFailure(String),
    #[error("leader is unknown yet")]
    LeaderUnknown,
    #[error("governor has stopped")]
    GovernorStopped,

    #[error("compare-and-swap: {0}")]
    Cas(#[from] crate::cas::Error),
    #[error("{0}")]
    Ddl(#[from] crate::schema::DdlError),

    #[error("sbroad: {0}")]
    Sbroad(#[from] sbroad::errors::SbroadError),

    #[error("transaction: {0}")]
    Transaction(String),

    #[error("storage corrupted: failed to decode field '{field}' from table '{table}'")]
    StorageCorrupted { table: String, field: String },

    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),

    #[error("{0}")]
    Other(Box<dyn std::error::Error>),
}

impl Error {
    #[inline(always)]
    pub fn other<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error>>,
    {
        Self::Other(error.into())
    }

    #[inline(always)]
    pub fn invalid_configuration(msg: impl ToString) -> Self {
        Self::InvalidConfiguration(msg.to_string())
    }

    /// Temporary solution until proc_cas returns structured errors
    #[inline(always)]
    pub fn is_cas_err(&self) -> bool {
        self.to_string().contains("compare-and-swap")
    }

    /// Temporary solution until proc_cas returns structured errors
    #[inline(always)]
    pub fn is_term_mismatch_err(&self) -> bool {
        self.to_string()
            .contains("operation request from different term")
    }

    /// Temporary solution until proc_cas returns structured errors
    #[inline(always)]
    pub fn is_not_leader_err(&self) -> bool {
        self.to_string().contains("not a leader")
    }

    #[inline(always)]
    pub fn is_retriable(&self) -> bool {
        is_retriable_error_message(&self.to_string())
    }
}

pub fn is_retriable_error_message(msg: &str) -> bool {
    if msg.contains("not a leader")
        || msg.contains("log unavailable")
        || msg.contains("operation request from different term")
    {
        return true;
    }

    if msg.contains("compare-and-swap") {
        return msg.contains("Compacted") || msg.contains("ConflictFound");
    }

    return false;
}

impl<E> From<timeout::Error<E>> for Error
where
    Error: From<E>,
{
    fn from(err: timeout::Error<E>) -> Self {
        match err {
            timeout::Error::Expired => Self::Timeout,
            timeout::Error::Failed(err) => err.into(),
        }
    }
}

impl From<::tarantool::network::Error> for Error {
    fn from(err: ::tarantool::network::Error) -> Self {
        Self::Tarantool(err.into())
    }
}

impl<E: Display> From<::tarantool::transaction::TransactionError<E>> for Error {
    fn from(err: ::tarantool::transaction::TransactionError<E>) -> Self {
        Self::Transaction(err.to_string())
    }
}

impl From<::tarantool::error::TarantoolError> for Error {
    fn from(err: ::tarantool::error::TarantoolError) -> Self {
        Self::Tarantool(err.into())
    }
}

impl<V> From<tarantool::tlua::CallError<V>> for Error
where
    V: Into<tarantool::tlua::Void>,
{
    fn from(err: tarantool::tlua::CallError<V>) -> Self {
        Self::Lua(err.into())
    }
}

impl IntoBoxError for Error {
    fn into_box_error(self) -> BoxError {
        self.to_string().into_box_error()
    }
}
