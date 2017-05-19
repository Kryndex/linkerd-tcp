use super::{DstAddr, DstConnection, DstCtx, Summary, Error};
use super::dispatcher::Dispatchee;
use super::super::Path;
use super::super::connection::Connection;
use super::super::connector::{Connector, Connecting};
use super::super::resolver::{self, Resolve};
use futures::{Future, Stream, Poll, Async};
use futures::unsync::{mpsc, oneshot};
use ordermap::OrderMap;
use rand::{self, Rng};
use std::{cmp, net};
use std::collections::VecDeque;
use tokio_core::reactor::Handle;

pub fn new(dst: Path,
           reactor: Handle,
           conn: Connector,
           min_conns: usize,
           dispatchees: mpsc::UnboundedReceiver<Dispatchee>)
           -> Manager {
    Manager {
        dst_name: dst,
        reactor: reactor,
        connector: conn,
        minimum_connections: min_conns,
        available: OrderMap::default(),
        retired: OrderMap::default(),
        dispatchees: dispatchees,
    }
}

pub struct Manager {
    dst_name: Path,

    reactor: Handle,

    connector: Connector,

    minimum_connections: usize,

    /// Endpoints considered available for new connections.
    available: OrderMap<net::SocketAddr, Endpoint>,

    /// Endpointsthat are still actvie but considered unavailable for new connections.
    retired: OrderMap<net::SocketAddr, Endpoint>,

    /// Requests from a `Dispatcher` for a `DstConnection`.
    dispatchees: mpsc::UnboundedReceiver<Dispatchee>,
}

type Completing = oneshot::Receiver<Summary>;

impl Manager {
    pub fn manage(self, resolve: Resolve) -> Managing {
        Managing {
            manager: self,
            resolve: resolve,
        }
    }

    fn dispatch(&mut self) {
        // If there are no endpoints to select from, we can't do anything.
        if self.available.is_empty() {
            // XXX we could fail these waiters here.  I'd prefer to rely on a timeout in
            // the dispatching task.
            return;
        }
        loop {
            match self.dispatchees.poll() {
                Err(_) |
                Ok(Async::NotReady) |
                Ok(Async::Ready(None)) => return,
                Ok(Async::Ready(Some(dispatchee))) => {
                    let mut ep = self.select_endpoint().unwrap();
                    ep.dispatch(dispatchee);
                }
            }
        }
    }

    fn select_endpoint(&mut self) -> Option<&mut Endpoint> {
        match self.available.len() {
            0 => {
                trace!("no endpoints ready");
                None
            }
            1 => {
                // One endpoint, use it.
                self.available.get_index_mut(0).map(|(_, ep)| ep)
            }
            sz => {
                // Pick 2 candidate indices.
                let (i0, i1) = if sz == 2 {
                    // There are only two endpoints, so no need for an RNG.
                    (0, 1)
                } else {
                    // 3 or more endpoints: choose two distinct endpoints at random.
                    let mut rng = rand::thread_rng();
                    let i0 = rng.gen_range(0, sz);
                    let mut i1 = rng.gen_range(0, sz);
                    while i0 == i1 {
                        i1 = rng.gen_range(0, sz);
                    }
                    (i0, i1)
                };
                let addr = {
                    // Determine the index of the lesser-loaded endpoint
                    let (addr0, ep0) = self.available.get_index(i0).unwrap();
                    let (addr1, ep1) = self.available.get_index(i1).unwrap();
                    if ep0.load <= ep1.load {
                        trace!("dst: {} *{} (not {} *{})",
                               addr0,
                               ep0.weight,
                               addr1,
                               ep1.weight);

                        *addr0
                    } else {
                        trace!("dst: {} *{} (not {} *{})",
                               addr1,
                               ep1.weight,
                               addr0,
                               ep0.weight);
                        *addr1
                    }
                };
                self.available.get_mut(&addr)
            }
        }
    }

    fn poll_connecting(&mut self) -> ConnectionPollSummary {
        let mut summary = ConnectionPollSummary::default();

        for ep in self.available.values_mut() {
            for _ in 0..ep.connecting.len() {
                let mut fut = ep.connecting.pop_front().unwrap();
                match fut.poll() {
                    Ok(Async::NotReady) => ep.connecting.push_back(fut),
                    Ok(Async::Ready(sock)) => {
                        let ctx = ep.mk_ctx(sock.local_addr());
                        let conn = Connection::new(self.dst_name.clone(), sock, ctx);
                        ep.connected.push_back(conn);
                    }
                    Err(_) => {
                        // XX ep.ctx.connect_fail();
                        summary.failed += 1
                    }
                }
            }
            summary.pending += ep.connecting.len();;
            summary.connected += ep.connected.len();
        }

        while !self.available.is_empty() &&
              summary.connected + summary.pending < self.minimum_connections {
            for mut ep in self.available.values_mut() {
                let mut fut = self.connector.connect(&ep.peer_addr, &self.reactor);
                // Poll the new connection immediately so that task notification is
                // established.
                match fut.poll() {
                    Ok(Async::NotReady) => {
                        // XXX ep.connecting.push_back(fut);
                        summary.pending += 1;
                    }
                    Ok(Async::Ready(sock)) => {
                        // XXX ep.ctx.connect_ok();
                        summary.connected += 1;
                        let ctx = ep.mk_ctx(sock.local_addr());
                        let conn = Connection::new(self.dst_name.clone(), sock, ctx);
                        ep.connected.push_back(conn)
                    }
                    Err(_) => {
                        // XXX ep.ctx.connect_fail();
                        summary.failed += 1
                    }
                }
                if summary.connected + summary.pending == self.minimum_connections {
                    break;
                }
            }
        }

        // self.established_connections = summary.connected;
        // self.pending_connections = summary.pending;
        summary
    }
    pub fn update_resolved(&mut self, resolved: resolver::Result<Vec<DstAddr>>) {
        if let Ok(ref resolved) = resolved {
            let mut dsts = OrderMap::with_capacity(resolved.len());
            for &DstAddr { addr, weight } in resolved {
                dsts.insert(addr, weight);
            }

            let mut temp = VecDeque::with_capacity(cmp::max(self.available.len(),
                                                            self.retired.len()));

            // Check retired endpoints.
            //
            // Endpoints are either salvaged backed into the active pool, maintained as
            // retired if still active, or dropped if inactive.
            {
                for (addr, ep) in self.retired.drain(..) {
                    if dsts.contains_key(&addr) {
                        self.available.insert(addr, ep);
                    } else if ep.is_idle() {
                        drop(ep);
                    } else {
                        temp.push_back((addr, ep));
                    }
                }
                for _ in 0..temp.len() {
                    let (addr, ep) = temp.pop_front().unwrap();
                    self.retired.insert(addr, ep);
                }
            }

            // Check active endpoints.
            //
            // Endpoints are either maintained in the active pool, moved into the retired poll if
            {
                for (addr, ep) in self.available.drain(..) {
                    if dsts.contains_key(&addr) {
                        temp.push_back((addr, ep));
                    } else if ep.is_idle() {
                        drop(ep);
                    } else {
                        // self.pending_connections -= ep.connecting.len();
                        // self.established_connections -= ep.connected.len();
                        self.retired.insert(addr, ep);
                    }
                }
                for _ in 0..temp.len() {
                    let (addr, ep) = temp.pop_front().unwrap();
                    self.available.insert(addr, ep);
                }
            }

            // Add new endpoints or update the base weights of existing endpoints.
            let name = self.dst_name.clone();
            for (addr, weight) in dsts.drain(..) {
                let mut ep = self.available
                    .entry(addr)
                    .or_insert_with(|| Endpoint::new(name.clone(), addr, weight));
                ep.weight = weight;
            }
        }
    }
}

struct Endpoint {
    dst_name: Path,
    peer_addr: net::SocketAddr,
    weight: f32,
    load: f32,

    /// Queues pending connections that have not yet been completed.
    connecting: VecDeque<Connecting>,

    /// Queues established connections that have not yet been dispatched.
    connected: VecDeque<DstConnection>,

    /// Queues dispatch requests for connections.
    dispatchees: VecDeque<Dispatchee>,

    /// Holds a future that will be completed when streaming is complete.
    ///
    /// ## XXX
    ///
    /// This shold be replaced with a notification-aware data structure so that all items
    /// are not polled regularly (so that balancers can scale to 100K+ connections).
    completing: VecDeque<Completing>,
}

impl Endpoint {
    fn new(dst: Path, addr: net::SocketAddr, weight: f32) -> Endpoint {
        Endpoint {
            dst_name: dst,
            peer_addr: addr,
            weight: weight,
            load: ::std::f32::MAX,
            connecting: VecDeque::default(),
            connected: VecDeque::default(),
            dispatchees: VecDeque::default(),
            completing: VecDeque::default(),
        }
    }

    fn mk_ctx(&mut self, local_addr: net::SocketAddr) -> DstCtx {
        let (tx, rx) = oneshot::channel();
        self.completing.push_back(rx);
        DstCtx::new(self.dst_name.clone(), local_addr, self.peer_addr, tx)
    }

    fn is_idle(&self) -> bool {
        // XXX this should
        self.connecting.is_empty() && self.dispatchees.is_empty()
    }

    // XXX
    // fn send_to_dispatchee(&mut self, conn: DstConnection) -> Result<(), DstConnection> {
    //     if let Some(waiter) = self.dispatchees.pop_front() {
    //         return match waiter.send(conn) {
    //                    Err(conn) => self.send_to_dispatchee(conn),
    //                    Ok(()) => Ok(()),
    //                };
    //     }
    //     Err(conn)
    // }

    fn dispatch(&mut self, d: Dispatchee) {
        match self.connected.pop_front() {
            None => self.dispatchees.push_back(d),
            Some(conn) => {
                if let Err(conn) = d.send(conn) {
                    // Dispatchee no longer waiting. save the connection for later.
                    self.connected.push_front(conn);
                }
            }
        }
    }
}

pub struct Managing {
    manager: Manager,
    resolve: Resolve,
}

/// Balancers accept a stream of service discovery updates,
impl Future for Managing {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Error> {
        // First, check new calls from the dispatcher.
        self.manager.dispatch();

        // Update the load balancer from service discovery.
        match self.resolve.poll() {
            Err(_) => error!("unexpected resolver error!"),
            Ok(Async::NotReady) => {}
            Ok(Async::Ready(None)) => {
                return Err(Error::ResolverLost());
            }
            Ok(Async::Ready(Some(resolved))) => {
                self.manager.update_resolved(resolved);
            }
        }

        let summary = self.manager.poll_connecting();
        trace!("start_send: Ready: {:?}", summary);

        Ok(Async::NotReady)
    }
}

#[derive(Debug, Default)]
struct ConnectionPollSummary {
    pending: usize,
    connected: usize,
    dispatched: usize,
    failed: usize,
}