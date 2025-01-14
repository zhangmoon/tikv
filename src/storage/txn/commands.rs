// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::fmt::{self, Debug, Display, Formatter};

use kvproto::kvrpcpb::{CommandPri, Context, GetRequest, RawGetRequest};
use tikv_util::collections::HashMap;
use txn_types::{Key, Lock, Mutation, TimeStamp};

use crate::storage::metrics::{self, CommandPriority};

/// Get a single value.
pub struct PointGetCommand {
    pub ctx: Context,
    pub key: Key,
    /// None if this is a raw get, Some if this is a transactional get.
    pub ts: Option<TimeStamp>,
}

impl PointGetCommand {
    pub fn from_get(request: &mut GetRequest) -> Self {
        PointGetCommand {
            ctx: request.take_context(),
            key: Key::from_raw(request.get_key()),
            ts: Some(request.get_version().into()),
        }
    }

    pub fn from_raw_get(request: &mut RawGetRequest) -> Self {
        PointGetCommand {
            ctx: request.take_context(),
            key: Key::from_raw(request.get_key()),
            ts: None,
        }
    }

    #[cfg(test)]
    pub fn from_key_ts(key: Key, ts: Option<TimeStamp>) -> Self {
        PointGetCommand {
            ctx: Context::default(),
            key,
            ts,
        }
    }
}

/// Store Transaction scheduler commands.
///
/// Learn more about our transaction system at
/// [Deep Dive TiKV: Distributed Transactions](https://tikv.org/deep-dive/distributed-transaction/)
///
/// These are typically scheduled and used through the [`Storage`](Storage) with functions like
/// [`Storage::async_prewrite`](Storage::async_prewrite) trait and are executed asyncronously.
// Logic related to these can be found in the `src/storage/txn/proccess.rs::process_write_impl` function.
pub struct Command {
    pub ctx: Context,
    pub kind: CommandKind,
}

pub enum CommandKind {
    /// The prewrite phase of a transaction. The first phase of 2PC.
    ///
    /// This prepares the system to commit the transaction. Later a [`Commit`](CommandKind::Commit)
    /// or a [`Rollback`](CommandKind::Rollback) should follow.
    ///
    /// If `options.for_update_ts` is `0`, the transaction is optimistic. Else it is pessimistic.
    Prewrite {
        /// The set of mutations to apply.
        mutations: Vec<Mutation>,
        /// The primary lock. Secondary locks (from `mutations`) will refer to the primary lock.
        primary: Vec<u8>,
        /// The transaction timestamp.
        start_ts: TimeStamp,
        options: Options,
    },
    /// Acquire a Pessimistic lock on the keys.
    ///
    /// This can be rolled back with a [`PessimisticRollback`](CommandKind::PessimisticRollback) command.
    AcquirePessimisticLock {
        /// The set of keys to lock.
        keys: Vec<(Key, bool)>,
        /// The primary lock. Secondary locks (from `keys`) will refer to the primary lock.
        primary: Vec<u8>,
        /// The transaction timestamp.
        start_ts: TimeStamp,
        options: Options,
    },
    /// Commit the transaction that started at `lock_ts`.
    ///
    /// This should be following a [`Prewrite`](CommandKind::Prewrite).
    Commit {
        /// The keys affected.
        keys: Vec<Key>,
        /// The lock timestamp.
        lock_ts: TimeStamp,
        /// The commit timestamp.
        commit_ts: TimeStamp,
    },
    /// Rollback mutations on a single key.
    ///
    /// This should be following a [`Prewrite`](CommandKind::Prewrite) on the given key.
    Cleanup {
        key: Key,
        /// The transaction timestamp.
        start_ts: TimeStamp,
        /// The approximate current ts when cleanup request is invoked, which is used to check the
        /// lock's TTL. 0 means do not check TTL.
        current_ts: TimeStamp,
    },
    /// Rollback from the transaction that was started at `start_ts`.
    ///
    /// This should be following a [`Prewrite`](CommandKind::Prewrite) on the given key.
    Rollback {
        keys: Vec<Key>,
        /// The transaction timestamp.
        start_ts: TimeStamp,
    },
    /// Rollback pessimistic locks identified by `start_ts` and `for_update_ts`.
    ///
    /// This can roll back an [`AcquirePessimisticLock`](CommandKind::AcquirePessimisticLock) command.
    PessimisticRollback {
        /// The keys to be rolled back.
        keys: Vec<Key>,
        /// The transaction timestamp.
        start_ts: TimeStamp,
        for_update_ts: TimeStamp,
    },
    /// Heart beat of a transaction. It enlarges the primary lock's TTL.
    ///
    /// This is invoked on a transaction's primary lock. The lock may be generated by either
    /// [`AcquirePessimisticLock`](CommandKind::AcquirePessimisticLock) or
    /// [`Prewrite`](CommandKind::Prewrite).
    TxnHeartBeat {
        /// The primary key of the transaction.
        primary_key: Key,
        /// The transaction's start_ts.
        start_ts: TimeStamp,
        /// The new TTL that will be used to update the lock's TTL. If the lock's TTL is already
        /// greater than `advise_ttl`, nothing will happen.
        advise_ttl: u64,
    },
    /// Check the status of a transaction. This is usually invoked by a transaction that meets
    /// another transaction's lock. If the primary lock is expired, it will rollback the primary
    /// lock. If the primary lock exists but is not expired, it may update the transaction's
    /// `min_commit_ts`. Returns a [`TxnStatus`](TxnStatus) to represent the status.
    ///
    /// This is invoked on a transaction's primary lock. The lock may be generated by either
    /// [`AcquirePessimisticLock`](CommandKind::AcquirePessimisticLock) or
    /// [`Prewrite`](CommandKind::Prewrite).
    CheckTxnStatus {
        /// The primary key of the transaction.
        primary_key: Key,
        /// The lock's ts, namely the transaction's start_ts.
        lock_ts: TimeStamp,
        /// The start_ts of the transaction that invokes this command.
        caller_start_ts: TimeStamp,
        /// The approximate current_ts when the command is invoked.
        current_ts: TimeStamp,
        /// Specifies the behavior when neither commit/rollback record nor lock is found. If true,
        /// rollbacks that transaction; otherwise returns an error.
        rollback_if_not_exist: bool,
    },
    /// Scan locks from `start_key`, and find all locks whose timestamp is before `max_ts`.
    ScanLock {
        /// The maximum transaction timestamp to scan.
        max_ts: TimeStamp,
        /// The key to start from. (`None` means start from the very beginning.)
        start_key: Option<Key>,
        /// The result limit.
        limit: usize,
    },
    /// Resolve locks according to `txn_status`.
    ///
    /// During the GC operation, this should be called to clean up stale locks whose timestamp is
    /// before safe point.
    ResolveLock {
        /// Maps lock_ts to commit_ts. If a transaction was rolled back, it is mapped to 0.
        ///
        /// For example, let `txn_status` be `{ 100: 101, 102: 0 }`, then it means that the transaction
        /// whose start_ts is 100 was committed with commit_ts `101`, and the transaction whose
        /// start_ts is 102 was rolled back. If there are these keys in the db:
        ///
        /// * "k1", lock_ts = 100
        /// * "k2", lock_ts = 102
        /// * "k3", lock_ts = 104
        /// * "k4", no lock
        ///
        /// Here `"k1"`, `"k2"` and `"k3"` each has a not-yet-committed version, because they have
        /// locks. After calling resolve_lock, `"k1"` will be committed with commit_ts = 101 and `"k2"`
        /// will be rolled back.  `"k3"` will not be affected, because its lock_ts is not contained in
        /// `txn_status`. `"k4"` will not be affected either, because it doesn't have a non-committed
        /// version.
        txn_status: HashMap<TimeStamp, TimeStamp>,
        scan_key: Option<Key>,
        key_locks: Vec<(Key, Lock)>,
    },
    /// Resolve locks on `resolve_keys` according to `start_ts` and `commit_ts`.
    ResolveLockLite {
        /// The transaction timestamp.
        start_ts: TimeStamp,
        /// The transaction commit timestamp.
        commit_ts: TimeStamp,
        /// The keys to resolve.
        resolve_keys: Vec<Key>,
    },
    /// Delete all keys in the range [`start_key`, `end_key`).
    ///
    /// **This is an unsafe action.**
    ///
    /// All keys in the range will be deleted permanently regardless of their timestamps.
    /// This means that deleted keys will not be retrievable by specifying an older timestamp.
    DeleteRange {
        /// The inclusive start key.
        start_key: Key,
        /// The exclusive end key.
        end_key: Key,
    },
    /// **Testing functionality:** Latch the given keys for given duration.
    ///
    /// This means other write operations that involve these keys will be blocked.
    Pause {
        /// The keys to hold latches on.
        keys: Vec<Key>,
        /// The amount of time in milliseconds to latch for.
        duration: u64,
    },
    /// Retrieve MVCC information for the given key.
    MvccByKey { key: Key },
    /// Retrieve MVCC info for the first committed key which `start_ts == ts`.
    MvccByStartTs { start_ts: TimeStamp },
}

impl Command {
    pub fn readonly(&self) -> bool {
        match self.kind {
            CommandKind::ScanLock { .. } |
            // DeleteRange only called by DDL bg thread after table is dropped and
            // must guarantee that there is no other read or write on these keys, so
            // we can treat DeleteRange as readonly Command.
            CommandKind::DeleteRange { .. } |
            CommandKind::MvccByKey { .. } |
            CommandKind::MvccByStartTs { .. } => true,
            CommandKind::ResolveLock { ref key_locks, .. } => key_locks.is_empty(),
            _ => false,
        }
    }

    pub fn priority(&self) -> CommandPri {
        self.ctx.get_priority()
    }

    pub fn is_sys_cmd(&self) -> bool {
        match self.kind {
            CommandKind::ScanLock { .. }
            | CommandKind::ResolveLock { .. }
            | CommandKind::ResolveLockLite { .. } => true,
            _ => false,
        }
    }

    pub fn priority_tag(&self) -> CommandPriority {
        get_priority_tag(self.ctx.get_priority())
    }

    pub fn need_flow_control(&self) -> bool {
        !self.readonly() && self.priority() != CommandPri::High
    }

    pub fn tag(&self) -> metrics::CommandKind {
        match self.kind {
            CommandKind::Prewrite { .. } => metrics::CommandKind::prewrite,
            CommandKind::AcquirePessimisticLock { .. } => {
                metrics::CommandKind::acquire_pessimistic_lock
            }
            CommandKind::Commit { .. } => metrics::CommandKind::commit,
            CommandKind::Cleanup { .. } => metrics::CommandKind::cleanup,
            CommandKind::Rollback { .. } => metrics::CommandKind::rollback,
            CommandKind::PessimisticRollback { .. } => metrics::CommandKind::pessimistic_rollback,
            CommandKind::TxnHeartBeat { .. } => metrics::CommandKind::txn_heart_beat,
            CommandKind::CheckTxnStatus { .. } => metrics::CommandKind::check_txn_status,
            CommandKind::ScanLock { .. } => metrics::CommandKind::scan_lock,
            CommandKind::ResolveLock { .. } => metrics::CommandKind::resolve_lock,
            CommandKind::ResolveLockLite { .. } => metrics::CommandKind::resolve_lock_lite,
            CommandKind::DeleteRange { .. } => metrics::CommandKind::delete_range,
            CommandKind::Pause { .. } => metrics::CommandKind::pause,
            CommandKind::MvccByKey { .. } => metrics::CommandKind::key_mvcc,
            CommandKind::MvccByStartTs { .. } => metrics::CommandKind::start_ts_mvcc,
        }
    }

    pub fn ts(&self) -> TimeStamp {
        match self.kind {
            CommandKind::Prewrite { start_ts, .. }
            | CommandKind::AcquirePessimisticLock { start_ts, .. }
            | CommandKind::Cleanup { start_ts, .. }
            | CommandKind::Rollback { start_ts, .. }
            | CommandKind::PessimisticRollback { start_ts, .. }
            | CommandKind::MvccByStartTs { start_ts, .. }
            | CommandKind::TxnHeartBeat { start_ts, .. } => start_ts,
            CommandKind::Commit { lock_ts, .. } | CommandKind::CheckTxnStatus { lock_ts, .. } => {
                lock_ts
            }
            CommandKind::ScanLock { max_ts, .. } => max_ts,
            CommandKind::ResolveLockLite { start_ts, .. } => start_ts,
            CommandKind::ResolveLock { .. }
            | CommandKind::DeleteRange { .. }
            | CommandKind::Pause { .. }
            | CommandKind::MvccByKey { .. } => TimeStamp::zero(),
        }
    }

    pub fn write_bytes(&self) -> usize {
        let mut bytes = 0;
        match self.kind {
            CommandKind::Prewrite { ref mutations, .. } => {
                for m in mutations {
                    match *m {
                        Mutation::Put((ref key, ref value))
                        | Mutation::Insert((ref key, ref value)) => {
                            bytes += key.as_encoded().len();
                            bytes += value.len();
                        }
                        Mutation::Delete(ref key) | Mutation::Lock(ref key) => {
                            bytes += key.as_encoded().len();
                        }
                    }
                }
            }
            CommandKind::AcquirePessimisticLock { ref keys, .. } => {
                for (key, _) in keys {
                    bytes += key.as_encoded().len();
                }
            }
            CommandKind::Commit { ref keys, .. }
            | CommandKind::Rollback { ref keys, .. }
            | CommandKind::PessimisticRollback { ref keys, .. }
            | CommandKind::Pause { ref keys, .. } => {
                for key in keys {
                    bytes += key.as_encoded().len();
                }
            }
            CommandKind::ResolveLock { ref key_locks, .. } => {
                for lock in key_locks {
                    bytes += lock.0.as_encoded().len();
                }
            }
            CommandKind::ResolveLockLite {
                ref resolve_keys, ..
            } => {
                for k in resolve_keys {
                    bytes += k.as_encoded().len();
                }
            }
            CommandKind::Cleanup { ref key, .. } => {
                bytes += key.as_encoded().len();
            }
            CommandKind::TxnHeartBeat {
                ref primary_key, ..
            } => {
                bytes += primary_key.as_encoded().len();
            }
            CommandKind::CheckTxnStatus {
                ref primary_key, ..
            } => {
                bytes += primary_key.as_encoded().len();
            }
            _ => {}
        }
        bytes
    }
}

impl Display for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.kind {
            CommandKind::Prewrite {
                ref mutations,
                start_ts,
                ..
            } => write!(
                f,
                "kv::command::prewrite mutations({}) @ {} | {:?}",
                mutations.len(),
                start_ts,
                self.ctx,
            ),
            CommandKind::AcquirePessimisticLock {
                ref keys,
                start_ts,
                ref options,
                ..
            } => write!(
                f,
                "kv::command::acquirepessimisticlock keys({}) @ {} {} | {:?}",
                keys.len(),
                start_ts,
                options.for_update_ts,
                self.ctx,
            ),
            CommandKind::Commit {
                ref keys,
                lock_ts,
                commit_ts,
                ..
            } => write!(
                f,
                "kv::command::commit {} {} -> {} | {:?}",
                keys.len(),
                lock_ts,
                commit_ts,
                self.ctx,
            ),
            CommandKind::Cleanup {
                ref key, start_ts, ..
            } => write!(
                f,
                "kv::command::cleanup {} @ {} | {:?}",
                key, start_ts, self.ctx
            ),
            CommandKind::Rollback {
                ref keys, start_ts, ..
            } => write!(
                f,
                "kv::command::rollback keys({}) @ {} | {:?}",
                keys.len(),
                start_ts,
                self.ctx,
            ),
            CommandKind::PessimisticRollback {
                ref keys,
                start_ts,
                for_update_ts,
            } => write!(
                f,
                "kv::command::pessimistic_rollback keys({}) @ {} {} | {:?}",
                keys.len(),
                start_ts,
                for_update_ts,
                self.ctx,
            ),
            CommandKind::TxnHeartBeat {
                ref primary_key,
                start_ts,
                advise_ttl,
            } => write!(
                f,
                "kv::command::txn_heart_beat {} @ {} ttl {} | {:?}",
                primary_key, start_ts, advise_ttl, self.ctx,
            ),
            CommandKind::CheckTxnStatus {
                ref primary_key,
                lock_ts,
                caller_start_ts,
                current_ts,
                ..
            } => write!(
                f,
                "kv::command::check_txn_status {} @ {} curr({}, {}) | {:?}",
                primary_key, lock_ts, caller_start_ts, current_ts, self.ctx,
            ),
            CommandKind::ScanLock {
                max_ts,
                ref start_key,
                limit,
                ..
            } => write!(
                f,
                "kv::scan_lock {:?} {} @ {} | {:?}",
                start_key, limit, max_ts, self.ctx,
            ),
            CommandKind::ResolveLock { .. } => write!(f, "kv::resolve_lock"),
            CommandKind::ResolveLockLite { .. } => write!(f, "kv::resolve_lock_lite"),
            CommandKind::DeleteRange {
                ref start_key,
                ref end_key,
            } => write!(
                f,
                "kv::command::delete range [{:?}, {:?}) | {:?}",
                start_key, end_key, self.ctx,
            ),
            CommandKind::Pause { ref keys, duration } => write!(
                f,
                "kv::command::pause keys:({}) {} ms | {:?}",
                keys.len(),
                duration,
                self.ctx,
            ),
            CommandKind::MvccByKey { ref key } => {
                write!(f, "kv::command::mvccbykey {:?} | {:?}", key, self.ctx)
            }
            CommandKind::MvccByStartTs { ref start_ts } => write!(
                f,
                "kv::command::mvccbystartts {:?} | {:?}",
                start_ts, self.ctx
            ),
        }
    }
}

impl Debug for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

pub fn get_priority_tag(priority: CommandPri) -> CommandPriority {
    match priority {
        CommandPri::Low => CommandPriority::low,
        CommandPri::Normal => CommandPriority::normal,
        CommandPri::High => CommandPriority::high,
    }
}

#[derive(Clone, Default)]
pub struct Options {
    pub lock_ttl: u64,
    pub skip_constraint_check: bool,
    pub key_only: bool,
    pub reverse_scan: bool,
    pub is_first_lock: bool,
    pub for_update_ts: TimeStamp,
    pub is_pessimistic_lock: Vec<bool>,
    // How many keys this transaction involved.
    pub txn_size: u64,
    pub min_commit_ts: TimeStamp,
    // Time to wait for lock released in milliseconds when encountering locks.
    // 0 means using default timeout. Negative means no wait.
    pub wait_timeout: i64,
}

impl Options {
    pub fn new(lock_ttl: u64, skip_constraint_check: bool, key_only: bool) -> Options {
        Options {
            lock_ttl,
            skip_constraint_check,
            key_only,
            ..Default::default()
        }
    }

    pub fn reverse_scan(mut self) -> Options {
        self.reverse_scan = true;
        self
    }
}
