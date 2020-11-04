use std::cmp::{max, min};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use std::{fmt, mem};

use futures_channel::oneshot;
use futures_util::stream::{FuturesUnordered, StreamExt};
use parking_lot::{Mutex, MutexGuard};
use tokio::spawn;
use tokio::time::{delay_for, interval_at, timeout, Interval};

use crate::api::{Builder, ManageConnection, RunError, State};

#[derive(Debug)]
pub(crate) struct Conn<C>
where
    C: Send,
{
    pub(crate) conn: C,
    birth: Instant,
}

impl<C: Send> From<IdleConn<C>> for Conn<C> {
    fn from(conn: IdleConn<C>) -> Self {
        conn.conn
    }
}

struct IdleConn<C>
where
    C: Send,
{
    conn: Conn<C>,
    idle_start: Instant,
}

impl<C: Send> From<Conn<C>> for IdleConn<C> {
    fn from(conn: Conn<C>) -> Self {
        IdleConn {
            conn,
            idle_start: Instant::now(),
        }
    }
}

/// The pool data that must be protected by a lock.
#[allow(missing_debug_implementations)]
struct PoolInternals<M>
where
    M: ManageConnection,
{
    waiters: VecDeque<oneshot::Sender<Conn<M::Connection>>>,
    conns: VecDeque<IdleConn<M::Connection>>,
    num_conns: u32,
    pending_conns: u32,
}

impl<M> PoolInternals<M>
where
    M: ManageConnection,
{
    fn pop(&mut self) -> Option<Conn<M::Connection>> {
        self.conns.pop_front().map(|idle| idle.conn)
    }

    fn put(&mut self, mut conn: IdleConn<M::Connection>, approval: Option<Approval>) {
        if approval.is_some() {
            self.pending_conns -= 1;
            self.num_conns += 1;
        }

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

    fn connect_failed(&mut self, _: Approval) {
        self.pending_conns -= 1;
    }

    fn dropped(&mut self, num: u32) {
        self.num_conns -= num;
    }

    fn wanted(&mut self, config: &Builder<M>) -> ApprovalIter {
        let available = self.conns.len() as u32 + self.pending_conns;
        let min_idle = config.min_idle.unwrap_or(0);
        let wanted = if available < min_idle {
            min_idle - available
        } else {
            0
        };

        self.approvals(&config, wanted)
    }

    fn push_waiter(
        &mut self,
        waiter: oneshot::Sender<Conn<M::Connection>>,
        config: &Builder<M>,
    ) -> ApprovalIter {
        self.waiters.push_back(waiter);
        self.approvals(config, 1)
    }

    fn approvals(&mut self, config: &Builder<M>, num: u32) -> ApprovalIter {
        let current = self.num_conns + self.pending_conns;
        let allowed = if current < config.max_size {
            config.max_size - current
        } else {
            0
        };

        let num = min(num, allowed);
        self.pending_conns += num;
        ApprovalIter { num: num as usize }
    }

    fn reap(&mut self, config: &Builder<M>) -> usize {
        let now = Instant::now();
        let before = self.conns.len();

        self.conns.retain(|conn| {
            let mut keep = true;
            if let Some(timeout) = config.idle_timeout {
                keep &= now - conn.idle_start < timeout;
            }
            if let Some(lifetime) = config.max_lifetime {
                keep &= now - conn.conn.birth < lifetime;
            }
            keep
        });

        before - self.conns.len()
    }

    pub(crate) fn state(&self) -> State {
        State {
            connections: self.num_conns,
            idle_connections: self.conns.len() as u32,
        }
    }
}

impl<M> Default for PoolInternals<M>
where
    M: ManageConnection,
{
    fn default() -> Self {
        Self {
            waiters: VecDeque::new(),
            conns: VecDeque::new(),
            num_conns: 0,
            pending_conns: 0,
        }
    }
}

pub(crate) struct PoolInner<M>
where
    M: ManageConnection + Send,
{
    inner: Arc<SharedPool<M>>,
}

impl<M> PoolInner<M>
where
    M: ManageConnection + Send,
{
    pub(crate) fn new(builder: Builder<M>, manager: M) -> Self {
        let inner = Arc::new(SharedPool {
            statics: builder,
            manager,
            internals: Mutex::new(PoolInternals::default()),
        });

        if inner.statics.max_lifetime.is_some() || inner.statics.idle_timeout.is_some() {
            let s = Arc::downgrade(&inner);
            if let Some(shared) = s.upgrade() {
                let start = Instant::now() + shared.statics.reaper_rate;
                let interval = interval_at(start.into(), shared.statics.reaper_rate);
                schedule_reaping(interval, s);
            }
        }

        Self { inner }
    }

    pub(crate) fn spawn_replenishing(&self) {
        let mut locked = self.inner.internals.lock();
        self.spawn_replenishing_approvals(locked.wanted(&self.inner.statics));
    }

    pub(crate) fn wanted(&self) -> ApprovalIter {
        self.inner.internals.lock().wanted(&self.inner.statics)
    }

    fn spawn_replenishing_locked(&self, locked: &mut MutexGuard<PoolInternals<M>>) {
        self.spawn_replenishing_approvals(locked.wanted(&self.inner.statics));
    }

    fn spawn_replenishing_approvals(&self, approvals: ApprovalIter) {
        if approvals.len() == 0 {
            return;
        }

        let this = self.clone();
        spawn(async move {
            let mut stream = this.replenish_idle_connections(approvals);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(()) => {}
                    Err(e) => this.inner.statics.error_sink.sink(e),
                }
            }
        });
    }

    pub(crate) fn replenish_idle_connections(
        &self,
        approvals: ApprovalIter,
    ) -> FuturesUnordered<impl Future<Output = Result<(), M::Error>>> {
        let stream = FuturesUnordered::new();
        for approval in approvals {
            let this = self.clone();
            stream.push(async move { this.add_connection(approval).await });
        }
        stream
    }

    pub(crate) async fn get(&self) -> Result<Conn<M::Connection>, RunError<M::Error>> {
        loop {
            let mut conn = {
                let mut locked = self.inner.internals.lock();
                match locked.pop() {
                    Some(conn) => {
                        self.spawn_replenishing_locked(&mut locked);
                        conn
                    }
                    None => break,
                }
            };

            if self.inner.statics.test_on_check_out {
                match self.inner.manager.is_valid(&mut conn.conn).await {
                    Ok(()) => return Ok(conn),
                    Err(_) => {
                        mem::drop(conn);
                        let mut internals = self.inner.internals.lock();
                        self.drop_connections(&mut internals, 1);
                    }
                }
                continue;
            } else {
                return Ok(conn);
            }
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut locked = self.inner.internals.lock();
            let approvals = locked.push_waiter(tx, &self.inner.statics);
            self.spawn_replenishing_approvals(approvals);
        };

        match timeout(self.inner.statics.connection_timeout, rx).await {
            Ok(Ok(conn)) => Ok(conn),
            _ => Err(RunError::TimedOut),
        }
    }

    pub(crate) async fn connect(&self) -> Result<M::Connection, M::Error> {
        self.inner.manager.connect().await
    }

    /// Return connection back in to the pool
    pub(crate) fn put_back(&self, mut conn: Conn<M::Connection>) {
        // Supposed to be fast, but do it before locking anyways.
        let broken = self.inner.manager.has_broken(&mut conn.conn);

        let mut locked = self.inner.internals.lock();
        if broken {
            self.drop_connections(&mut locked, 1);
        } else {
            locked.put(conn.into(), None);
        }
    }

    /// Returns information about the current state of the pool.
    pub(crate) fn state(&self) -> State {
        self.inner.internals.lock().state()
    }

    fn reap(&self) {
        let mut internals = self.inner.internals.lock();
        let dropped = internals.reap(&self.inner.statics);
        self.drop_connections(&mut internals, dropped);
    }

    // Outside of Pool to avoid borrow splitting issues on self
    async fn add_connection(&self, approval: Approval) -> Result<(), M::Error>
    where
        M: ManageConnection,
    {
        let new_shared = Arc::downgrade(&self.inner);
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

                    shared.internals.lock().put(conn, Some(approval));
                    return Ok(());
                }
                Err(e) => {
                    if Instant::now() - start > self.inner.statics.connection_timeout {
                        let mut locked = shared.internals.lock();
                        locked.connect_failed(approval);
                        return Err(e);
                    } else {
                        delay = max(Duration::from_millis(200), delay);
                        delay = min(self.inner.statics.connection_timeout / 2, delay * 2);
                        delay_for(delay).await;
                    }
                }
            }
        }
    }

    // Drop connections
    // NB: This is called with the pool lock held.
    fn drop_connections(&self, internals: &mut MutexGuard<PoolInternals<M>>, dropped: usize)
    where
        M: ManageConnection,
    {
        internals.dropped(dropped as u32);
        // We might need to spin up more connections to maintain the idle limit, e.g.
        // if we hit connection lifetime limits
        self.spawn_replenishing_locked(internals);
    }
}

impl<M> Clone for PoolInner<M>
where
    M: ManageConnection,
{
    fn clone(&self) -> Self {
        PoolInner {
            inner: self.inner.clone(),
        }
    }
}

impl<M> fmt::Debug for PoolInner<M>
where
    M: ManageConnection,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!("PoolInner({:p})", self.inner))
    }
}

/// The guts of a `Pool`.
#[allow(missing_debug_implementations)]
struct SharedPool<M>
where
    M: ManageConnection + Send,
{
    statics: Builder<M>,
    manager: M,
    internals: Mutex<PoolInternals<M>>,
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

fn schedule_reaping<M>(mut interval: Interval, weak_shared: Weak<SharedPool<M>>)
where
    M: ManageConnection,
{
    spawn(async move {
        loop {
            let _ = interval.tick().await;
            if let Some(inner) = weak_shared.upgrade() {
                PoolInner { inner }.reap()
            } else {
                break;
            }
        }
    });
}
