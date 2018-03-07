use std::net::{SocketAddr};
use std::sync::Arc;

use http;
use tower;
use tower_buffer::{self, Buffer};
use tower_in_flight_limit::{self, InFlightLimit};
use tower_h2;
use conduit_proxy_router::Recognize;

use bind;
use ctx;
use time::Timer;

type Bind<B, T> = bind::Bind<Arc<ctx::Proxy>, B, T>;

pub struct Inbound<B, T> {
    default_addr: Option<SocketAddr>,
    bind: Bind<B, T>,
}

const MAX_IN_FLIGHT: usize = 10_000;

// ===== impl Inbound =====

impl<B, T> Inbound<B, T> {
    pub fn new(default_addr: Option<SocketAddr>, bind: Bind<B, T>) -> Self {
        Self {
            default_addr,
            bind,
        }
    }
}

impl<B, T> Recognize for Inbound<B, T>
where
    B: tower_h2::Body + 'static,
    T: Timer + 'static,
{
    type Request = http::Request<B>;
    type Response = bind::HttpResponse<T>;
    type Error = tower_in_flight_limit::Error<
        tower_buffer::Error<
            <bind::Service<B, T> as tower::Service>::Error
        >
    >;
    type Key = (SocketAddr, bind::Protocol);
    type RouteError = bind::BufferSpawnError;
    type Service = InFlightLimit<Buffer<bind::Service<B, T>>>;

    fn recognize(&self, req: &Self::Request) -> Option<Self::Key> {
        let key = req.extensions()
            .get::<Arc<ctx::transport::Server>>()
            .and_then(|ctx| {
                trace!("recognize local={} orig={:?}", ctx.local, ctx.orig_dst);
                ctx.orig_dst_if_not_local()
            })
            .or_else(|| self.default_addr);

        let proto = match req.version() {
            http::Version::HTTP_2 => bind::Protocol::Http2,
            _ => bind::Protocol::Http1,
        };

        let key = key.map(|addr| (addr, proto));

        trace!("recognize key={:?}", key);

        key
    }

    /// Builds a static service to a single endpoint.
    ///
    /// # TODO
    ///
    /// Buffering is currently unbounded and does not apply timeouts. This must be
    /// changed.
    fn bind_service(&mut self, key: &Self::Key) -> Result<Self::Service, Self::RouteError> {
        let &(ref addr, proto) = key;
        debug!("building inbound {:?} client to {}", proto, addr);

        Buffer::new(self.bind.bind_service(addr, proto), self.bind.executor())
            .map(|buffer| {
                InFlightLimit::new(buffer, MAX_IN_FLIGHT)
            })
            .map_err(|_| bind::BufferSpawnError::Inbound)
    }
}

#[cfg(test)]
mod tests {
    use std::net;
    use std::sync::Arc;

    use http;
    use tokio_core::reactor::{Core, Handle};
    use conduit_proxy_router::Recognize;

    use super::Inbound;
    use conduit_proxy_controller_grpc::common::Protocol;
    use bind::{self, Bind};
    use ctx;

    fn new_inbound(default: Option<net::SocketAddr>, ctx: &Arc<ctx::Proxy>)
                  -> Inbound<(), Handle>
    {
        let core = Core::new().unwrap();
        let timer = core.handle();
        let bind = Bind::new(core.handle(), timer).with_ctx(ctx.clone());
        Inbound::new(default, bind)
    }

    quickcheck! {
        fn recognize_orig_dst(
            orig_dst: net::SocketAddr,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let ctx = ctx::Proxy::inbound(&ctx::Process::test("test", "test", "test"));

            let inbound = new_inbound(None, &ctx);

            let srv_ctx = ctx::transport::Server::new(&ctx, &local, &remote, &Some(orig_dst), Protocol::Http);

            let rec = srv_ctx.orig_dst_if_not_local().map(|addr| (addr, bind::Protocol::Http1));

            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(srv_ctx);

            inbound.recognize(&req) == rec
        }

        fn recognize_default_no_orig_dst(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let ctx = ctx::Proxy::inbound(&ctx::Process::test("test", "test", "test"));

            let inbound = new_inbound(default, &ctx);

            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(ctx::transport::Server::new(
                    &ctx,
                    &local,
                    &remote,
                    &None,
                    Protocol::Http,
                ));

            inbound.recognize(&req) == default.map(|addr| (addr, bind::Protocol::Http1))
        }

        fn recognize_default_no_ctx(default: Option<net::SocketAddr>) -> bool {
            let ctx = ctx::Proxy::inbound(&ctx::Process::test("test", "test", "test"));

            let inbound = new_inbound(default, &ctx);

            let req = http::Request::new(());

            inbound.recognize(&req) == default.map(|addr| (addr, bind::Protocol::Http1))
        }

        fn recognize_default_no_loop(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let ctx = ctx::Proxy::inbound(&ctx::Process::test("test", "test", "test"));

            let inbound = new_inbound(default, &ctx);

            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(ctx::transport::Server::new(
                    &ctx,
                    &local,
                    &remote,
                    &Some(local),
                    Protocol::Http,
                ));

            inbound.recognize(&req) == default.map(|addr| (addr, bind::Protocol::Http1))
        }
    }
}
