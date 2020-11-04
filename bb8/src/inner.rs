use std::cmp::{max, min};
use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use parking_lot::{Mutex, MutexGuard};
use tokio::spawn;
use tokio::time::{delay_for, Interval};

use crate::api::{Builder, ManageConnection, Pool};

#[derive(Debug)]
pub(crate) struct Conn<C>
where
    C: Send,
{
    pub(crate) conn: C,
    pub(crate) birth: Instant,
}

pub(crate) struct IdleConn<C>
where
    C: Send,
{
    pub(crate) conn: Conn<C>,
    pub(crate) idle_start: Instant,
}

impl<C> IdleConn<C>
where
    C: Send,
{
    pub(crate) fn make_idle(conn: Conn<C>) -> IdleConn<C> {
        let now = Instant::now();
        IdleConn {
            conn,
            idle_start: now,
        }
    }
}

/// The pool data that must be protected by a lock.
#[allow(missing_debug_implementations)]
pub(crate) struct PoolInternals<C>
where
    C: Send,
{
    pub(crate) waiters: VecDeque<oneshot::Sender<Conn<C>>>,
    pub(crate) conns: VecDeque<IdleConn<C>>,
    pub(crate) num_conns: u32,
    pub(crate) pending_conns: u32,
}

impl<C> PoolInternals<C>
where
    C: Send,
{
    pub(crate) fn put_idle_conn(&mut self, mut conn: IdleConn<C>) {
        loop {
            if let Some(waiter) = self.waiters.pop_front() {
                // This connection is no longer idle, send it back out.
                match waiter.send(conn.conn) {
                    Ok(_) => break,
                    // Oops, that receiver was gone. Loop and try again.
                    Err(c) => conn.conn = c,
                }
            } else {
                // Queue it in the idle queue.
                self.conns.push_back(conn);
                break;
            }
        }
    }
}

/// The guts of a `Pool`.
#[allow(missing_debug_implementations)]
pub(crate) struct SharedPool<M>
where
    M: ManageConnection + Send,
{
    pub(crate) statics: Builder<M>,
    pub(crate) manager: M,
    pub(crate) internals: Mutex<PoolInternals<M::Connection>>,
}

impl<M> SharedPool<M>
where
    M: ManageConnection,
{
    pub(crate) fn get(&self) -> Option<(IdleConn<M::Connection>, ApprovalIter)> {
        let mut locked = self.internals.lock();
        locked.conns.pop_front().map(|conn| {
            let wanted = self.want_more(&mut locked);
            (conn, self.approvals(&mut locked, wanted))
        })
    }

    pub(crate) fn can_add_more(
        &self,
        waiter: Option<oneshot::Sender<Conn<M::Connection>>>,
    ) -> Option<Approval> {
        let mut locked = self.internals.lock();
        if let Some(waiter) = waiter {
            locked.waiters.push_back(waiter);
        }

        if locked.num_conns + locked.pending_conns < self.statics.max_size {
            locked.pending_conns += 1;
            Some(Approval { _priv: () })
        } else {
            None
        }
    }

    pub(crate) fn wanted(&self) -> ApprovalIter {
        let mut internals = self.internals.lock();
        let num = self.want_more(&mut internals);
        self.approvals(&mut internals, num)
    }

    fn want_more(&self, locked: &mut MutexGuard<PoolInternals<M::Connection>>) -> u32 {
        let available = locked.conns.len() as u32 + locked.pending_conns;
        let min_idle = self.statics.min_idle.unwrap_or(0);
        if available < min_idle {
            min_idle - available
        } else {
            0
        }
    }

    fn approvals(
        &self,
        locked: &mut MutexGuard<PoolInternals<M::Connection>>,
        num: u32,
    ) -> ApprovalIter {
        let current = locked.num_conns + locked.pending_conns;
        let allowed = if current < self.statics.max_size {
            self.statics.max_size - current
        } else {
            0
        };

        let num = min(num, allowed);
        locked.pending_conns += num;
        ApprovalIter { num: num as usize }
    }

    pub(crate) fn sink_error(&self, result: Result<(), M::Error>) {
        match result {
            Ok(()) => {}
            Err(e) => self.statics.error_sink.sink(e),
        }
    }
}

// Outside of Pool to avoid borrow splitting issues on self
pub(crate) async fn add_connection<M>(pool: Arc<SharedPool<M>>, _: Approval) -> Result<(), M::Error>
where
    M: ManageConnection,
{
    let new_shared = Arc::downgrade(&pool);
    let shared = match new_shared.upgrade() {
        None => return Ok(()),
        Some(shared) => shared,
    };

    let start = Instant::now();
    let mut delay = Duration::from_secs(0);
    loop {
        match shared.manager.connect().await {
            Ok(conn) => {
                let now = Instant::now();
                let conn = IdleConn {
                    conn: Conn { conn, birth: now },
                    idle_start: now,
                };

                let mut locked = shared.internals.lock();
                locked.pending_conns -= 1;
                locked.num_conns += 1;
                locked.put_idle_conn(conn);
                return Ok(());
            }
            Err(e) => {
                if Instant::now() - start > pool.statics.connection_timeout {
                    let mut locked = shared.internals.lock();
                    locked.pending_conns -= 1;
                    return Err(e);
                } else {
                    delay = max(Duration::from_millis(200), delay);
                    delay = min(pool.statics.connection_timeout / 2, delay * 2);
                    delay_for(delay).await;
                }
            }
        }
    }
}

pub(crate) struct ApprovalIter {
    num: usize,
}

impl Iterator for ApprovalIter {
    type Item = Approval;

    fn next(&mut self) -> Option<Self::Item> {
        match self.num {
            0 => None,
            _ => {
                self.num -= 1;
                Some(Approval { _priv: () })
            }
        }
    }
}

impl ExactSizeIterator for ApprovalIter {
    fn len(&self) -> usize {
        self.num
    }
}

pub(crate) struct Approval {
    _priv: (),
}

// Drop connections
// NB: This is called with the pool lock held.
pub(crate) fn drop_connections<'a, M>(
    pool: &Arc<SharedPool<M>>,
    internals: &mut MutexGuard<'a, PoolInternals<M::Connection>>,
    dropped: usize,
) where
    M: ManageConnection,
{
    internals.num_conns -= dropped as u32;
    // We might need to spin up more connections to maintain the idle limit, e.g.
    // if we hit connection lifetime limits
    let num = pool.want_more(internals);
    if num > 0 {
        Pool {
            inner: pool.clone(),
        }
        .spawn_replenishing(pool.approvals(internals, num));
    }
}

pub(crate) fn schedule_reaping<M>(mut interval: Interval, weak_shared: Weak<SharedPool<M>>)
where
    M: ManageConnection,
{
    spawn(async move {
        loop {
            let _ = interval.tick().await;
            if let Some(pool) = weak_shared.upgrade() {
                let mut internals = pool.internals.lock();
                let now = Instant::now();
                let before = internals.conns.len();

                internals.conns.retain(|conn| {
                    let mut keep = true;
                    if let Some(timeout) = pool.statics.idle_timeout {
                        keep &= now - conn.idle_start < timeout;
                    }
                    if let Some(lifetime) = pool.statics.max_lifetime {
                        keep &= now - conn.conn.birth < lifetime;
                    }
                    keep
                });

                let dropped = before - internals.conns.len();
                drop_connections(&pool, &mut internals, dropped);
            } else {
                break;
            }
        }
    });
}
