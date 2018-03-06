use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use support::*;

pub fn new() -> Server {
    http2()
}

pub fn http1() -> Server {
    Server::http1()
}

pub fn http2() -> Server {
    Server::http2()
}

pub fn tcp() -> tcp::TcpServer {
    tcp::server()
}

#[derive(Debug)]
pub struct Server {
    routes: HashMap<String, Route>,
    version: Run,
}

#[derive(Debug)]
pub struct Listening {
    pub addr: SocketAddr,
    pub(super) shutdown: Shutdown,
}

impl Server {
    fn new(run: Run) -> Self {
        Server {
            routes: HashMap::new(),
            version: run,
        }
    }
    fn http1() -> Self {
        Server::new(Run::Http1)
    }

    fn http2() -> Self {
        Server::new(Run::Http2)
    }

    pub fn route(mut self, path: &str, resp: &str) -> Self {
        self.routes.insert(path.into(), Route::string(resp));
        self
    }

    pub fn route_fn<F>(mut self, path: &str, cb: F) -> Self
    where
        F: Fn(Request<()>) -> Response<String> + Send + 'static,
    {
        self.routes.insert(path.into(), Route(Box::new(cb)));
        self
    }

    pub fn route_with_latency(
        mut self,
        path: &str,
        resp: &str,
        time: timer::Control,
        latency: Duration
    ) -> Self {
        let resp = resp.to_owned();

        let route = Route(Box::new(move |_| {
            let mut time = time.clone();
            time += latency;
            http::Response::builder()
                .status(200)
                .body(resp.clone())
                .unwrap()
        }));
        self.routes.insert(path.into(), route);
        self
    }

    pub fn run(self) -> Listening {
        let (tx, rx) = shutdown_signal();
        let (addr_tx, addr_rx) = oneshot::channel();
        ::std::thread::Builder::new().name("support server".into()).spawn(move || {
            let mut core = Core::new().unwrap();
            let reactor = core.handle();

            let new_svc = NewSvc(Arc::new(self.routes));

            let srv: Box<Fn(TcpStream) -> Box<Future<Item=(), Error=()>>> = match self.version {
                Run::Http1 => {
                    let h1 = hyper::server::Http::<hyper::Chunk>::new();

                    Box::new(move |sock| {
                        let h1_clone = h1.clone();
                        let conn = new_svc.new_service()
                            .from_err()
                            .and_then(move |svc| h1_clone.serve_connection(sock, svc))
                            .map(|_| ())
                            .map_err(|e| println!("server h1 error: {}", e));
                        Box::new(conn)
                    })
                },
                Run::Http2 => {
                    let h2 = tower_h2::Server::new(
                        new_svc,
                        Default::default(),
                        reactor.clone(),
                    );
                    Box::new(move |sock| {
                        let conn = h2.serve(sock)
                            .map_err(|e| println!("server h2 error: {:?}", e));
                        Box::new(conn)
                    })
                },
            };

            let addr = ([127, 0, 0, 1], 0).into();
            let bind = TcpListener::bind(&addr, &reactor).expect("bind");

            let local_addr = bind.local_addr().expect("local_addr");
            let _ = addr_tx.send(local_addr);

            let serve = bind.incoming()
                .fold((srv, reactor), |(srv, reactor), (sock, _)| {
                    if let Err(e) = sock.set_nodelay(true) {
                        return Err(e);
                    }
                    reactor.spawn(srv(sock));

                    Ok((srv, reactor))
                });

            core.handle().spawn(
                serve
                    .map(|_| ())
                    .map_err(|e| println!("server error: {}", e)),
            );

            core.run(rx).unwrap();
        }).unwrap();

        let addr = addr_rx.wait().expect("addr");

        Listening {
            addr,
            shutdown: tx,
        }
    }
}

#[derive(Debug)]
enum Run {
    Http1,
    Http2,
}

struct RspBody(Option<Bytes>);

impl RspBody {
    fn new(body: Bytes) -> Self {
        RspBody(Some(body))
    }

    fn empty() -> Self {
        RspBody(None)
    }
}


impl Body for RspBody {
    type Data = Bytes;

    fn is_end_stream(&self) -> bool {
        self.0.as_ref().map(|b| b.is_empty()).unwrap_or(false)
    }

    fn poll_data(&mut self) -> Poll<Option<Bytes>, h2::Error> {
        let data = self.0
            .take()
            .and_then(|b| if b.is_empty() { None } else { Some(b) });
        Ok(Async::Ready(data))
    }
}

struct Route(Box<Fn(Request<()>) -> Response<String> + Send>);

impl Route {
    fn string(body: &str) -> Route {
        let body = body.to_owned();
        Route(Box::new(move |_| {
            http::Response::builder()
                .status(200)
                .body(body.clone())
                .unwrap()
        }))
    }
}

impl ::std::fmt::Debug for Route {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        f.write_str("Route")
    }
}

#[derive(Debug)]
struct Svc(Arc<HashMap<String, Route>>);

impl Service for Svc {
    type Request = Request<RecvBody>;
    type Response = Response<RspBody>;
    type Error = h2::Error;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let rsp = match self.0.get(req.uri().path()) {
            Some(route) => {
                (route.0)(req.map(|_| ()))
                    .map(|s| RspBody::new(s.as_bytes().into()))
            }
            None => {
                println!("server 404: {:?}", req.uri().path());
                let mut rsp = http::Response::builder();
                rsp.version(http::Version::HTTP_2);
                let body = RspBody::empty();
                rsp.status(404).body(body).unwrap()
            }
        };
        future::ok(rsp)
    }
}

impl hyper::server::Service for Svc {
    type Request = hyper::server::Request;
    type Response = hyper::server::Response<hyper::Body>;
    type Error = hyper::Error;
    type Future = future::FutureResult<hyper::server::Response<hyper::Body>, hyper::Error>;

    fn call(&self, req: Self::Request) -> Self::Future {

        let rsp = match self.0.get(req.uri().path()) {
            Some(route) => {
                (route.0)(Request::from(req).map(|_| ()))
                    .map(|s| hyper::Body::from(s))
                    .into()
            }
            None => {
                println!("server 404: {:?}", req.uri().path());
                let rsp = hyper::server::Response::new();
                let body = hyper::Body::empty();
                rsp.with_status(hyper::NotFound)
                    .with_body(body)
            }
        };
        future::ok(rsp)
    }
}

#[derive(Debug)]
struct NewSvc(Arc<HashMap<String, Route>>);
impl NewService for NewSvc {
    type Request = Request<RecvBody>;
    type Response = Response<RspBody>;
    type Error = h2::Error;
    type InitError = ::std::io::Error;
    type Service = Svc;
    type Future = future::FutureResult<Svc, Self::InitError>;

    fn new_service(&self) -> Self::Future {
        future::ok(Svc(Arc::clone(&self.0)))
    }
}
