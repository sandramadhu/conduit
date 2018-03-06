use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{future, Async, Future, Poll, Stream};
use h2;
use http;
use tokio_core::reactor::{
    Handle,
    // TODO: would rather just have Backoff in a separate file so this
    //       renaming import is not necessary.
    Timeout as ReactorTimeout
};
use tower::Service;
use tower_h2;
use tower_reconnect::{Error as ReconnectError, Reconnect};

use dns;
use fully_qualified_authority::FullyQualifiedAuthority;
use transport::{HostAndPort, LookupAddressAndConnect};
use time::{NewTimeout, Timer, TimeoutError};

pub mod discovery;
mod observe;
pub mod pb;
mod telemetry;

use self::discovery::{Background as DiscoBg, Discovery, Watch};
pub use self::discovery::Bind;
pub use self::observe::Observe;
use conduit_proxy_controller_grpc::telemetry::ReportRequest;
use self::telemetry::Telemetry;

pub struct Control {
    disco: Discovery,
}

pub struct Background<T> {
    disco: DiscoBg,
    connect_timeout: NewTimeout<T>,
}

pub fn new<T: Timer>(timer: &T) -> (Control, Background<T>) {
    let (tx, rx) = self::discovery::new();

    let c = Control {
        disco: tx,
    };

    let connect_timeout = timer
        .new_timeout(Duration::from_secs(3))
        .with_description("discovery connection")
        ;

    let b = Background {
        disco: rx,
        connect_timeout,
    };

    (c, b)
}

// ===== impl Control =====

impl Control {
    pub fn resolve<B>(&self, auth: &FullyQualifiedAuthority, bind: B) -> Watch<B> {
        self.disco.resolve(auth, bind)
    }
}

// ===== impl Background =====

impl<T> Background<T>
where
    T: Timer + 'static,
    T::Error: Error,
{
    pub fn bind<S>(
        self,
        events: S,
        host_and_port: HostAndPort,
        dns_config: dns::Config,
        report_timeout: Duration,
        executor: &Handle,
    ) -> Box<Future<Item = (), Error = ()>>
    where
        S: Stream<Item = ReportRequest, Error = ()> + 'static,
    {
        // Build up the Controller Client Stack
        let mut client = {
            let ctx = ("controller-client", format!("{:?}", host_and_port));
            let scheme = http::uri::Scheme::from_shared(Bytes::from_static(b"http")).unwrap();
            let authority = http::uri::Authority::from(&host_and_port);
            let dns_resolver = dns::Resolver::new(dns_config, executor);
            let connect = self.connect_timeout.apply_to(
                LookupAddressAndConnect::new(host_and_port, dns_resolver, executor),
            );

            let h2_client = tower_h2::client::Connect::new(
                connect,
                h2::client::Builder::default(),
                ::logging::context_executor(ctx, executor.clone()),
            );


            let reconnect = Reconnect::new(h2_client);
            let log_errors = LogErrors::new(reconnect);
            let backoff = Backoff::new(log_errors, Duration::from_secs(5), executor);
            // TODO: Use AddOrigin in tower-http
            AddOrigin::new(scheme, authority, backoff)
        };

        let mut disco = self.disco.work();
        let mut telemetry = Telemetry::new(
            events,
            report_timeout,
            self.connect_timeout.timer()
        );

        let fut = future::poll_fn(move || {
            disco.poll_rpc(&mut client);
            telemetry.poll_rpc(&mut client);

            Ok(Async::NotReady)
        });

        Box::new(fut)
    }
}

// ===== Backoff =====

/// Wait a duration if inner `poll_ready` returns an error.
//TODO: move to tower-backoff
struct Backoff<S> {
    inner: S,
    timer: ReactorTimeout,
    waiting: bool,
    wait_dur: Duration,
}

impl<S> Backoff<S> {
    fn new(inner: S, wait_dur: Duration, handle: &Handle) -> Self {
        Backoff {
            inner,
            timer: ReactorTimeout::new(wait_dur, handle).unwrap(),
            waiting: false,
            wait_dur,
        }
    }
}

impl<S> Service for Backoff<S>
where
    S: Service,
    S::Error: ::std::fmt::Debug,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.waiting {
            if self.timer.poll().unwrap().is_not_ready() {
                return Ok(Async::NotReady);
            }

            self.waiting = false;
        }

        match self.inner.poll_ready() {
            Err(_err) => {
                trace!("backoff: controller error, waiting {:?}", self.wait_dur);
                self.waiting = true;
                self.timer.reset(Instant::now() + self.wait_dur);
                Ok(Async::NotReady)
            }
            ok => ok,
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.inner.call(req)
    }
}

/// Wraps an HTTP service, injecting authority and scheme on every request.
struct AddOrigin<S> {
    authority: http::uri::Authority,
    inner: S,
    scheme: http::uri::Scheme,
}

impl<S> AddOrigin<S> {
    fn new(scheme: http::uri::Scheme, auth: http::uri::Authority, service: S) -> Self {
        AddOrigin {
            authority: auth,
            inner: service,
            scheme,
        }
    }
}

impl<S, B> Service for AddOrigin<S>
where
    S: Service<Request = http::Request<B>>,
{
    type Request = http::Request<B>;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let (mut head, body) = req.into_parts();
        let mut uri: http::uri::Parts = head.uri.into();
        uri.scheme = Some(self.scheme.clone());
        uri.authority = Some(self.authority.clone());
        head.uri = http::Uri::from_parts(uri).expect("valid uri");

        self.inner.call(http::Request::from_parts(head, body))
    }
}

// ===== impl LogErrors

/// Log errors talking to the controller in human format.
struct LogErrors<S> {
    inner: S,
}

// We want some friendly logs, but the stack of services don't have fmt::Display
// errors, so we have to build that ourselves. For now, this hard codes the
// expected error stack, and so any new middleware added will need to adjust this.
//
// The dead_code allowance is because rustc is being stupid and doesn't see it
// is used down below.
#[allow(dead_code)]
type LogError<T, U> = ReconnectError<
    tower_h2::client::Error,
    tower_h2::client::ConnectError<
        TimeoutError<
            T,
            U,
        >
    >
>;

impl<S, T, U> LogErrors<S>
where
    S: Service<Error=LogError<T, U>>,
{
    fn new(service: S) -> Self {
        LogErrors {
            inner: service,
        }
    }
}

impl<S, T, U> Service for LogErrors<S>
where
    S: Service<Error=LogError<T, U>>,
    T: Error,
    U: Error
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(|e| {
            error!("controller error: {}", HumanError(&e));
            e
        })
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.inner.call(req)
    }
}

struct HumanError<'a, T: 'a, U: 'a>(&'a LogError<T, U>);

impl<'a, T, U> fmt::Display for HumanError<'a, T, U>
where
    TimeoutError<T, U>: Error,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self.0 {
            ReconnectError::Inner(ref e) => {
                fmt::Display::fmt(e, f)
            },
            ReconnectError::Connect(ref e) => {
                fmt::Display::fmt(e, f)
            },
            ReconnectError::NotReady => {
                // this error should only happen if we `call` the service
                // when it isn't ready, which is really more of a bug on
                // our side...
                f.pad("bug: called service when not ready")
            },
        }
    }
}
