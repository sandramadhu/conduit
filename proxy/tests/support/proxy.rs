use support::*;
use support::time::{Timer, LazyReactorTimer};

use std::error::Error;
use std::sync::{Arc, Mutex};

use convert::TryFrom;

pub fn new() -> Proxy<LazyReactorTimer> {
    Proxy::new()
}

#[derive(Debug)]
pub struct Proxy<T> {
    controller: Option<controller::Listening>,
    inbound: Option<server::Listening>,
    outbound: Option<server::Listening>,

    metrics_flush_interval: Option<Duration>,

    timer: T,
}

#[derive(Debug)]
pub struct Listening {
    pub control: SocketAddr,
    pub inbound: SocketAddr,
    pub outbound: SocketAddr,

    shutdown: Shutdown,
}

impl Proxy<LazyReactorTimer> {
    pub fn new() -> Self {
        Proxy {
            controller: None,
            inbound: None,
            outbound: None,

            metrics_flush_interval: None,

            timer: LazyReactorTimer::uninitialized(),
        }
    }
}

impl<T> Proxy<T> {

    pub fn controller(mut self, c: controller::Listening) -> Self {
        self.controller = Some(c);
        self
    }

    pub fn inbound(mut self, s: server::Listening) -> Self {
        self.inbound = Some(s);
        self
    }

    pub fn outbound(mut self, s: server::Listening) -> Self {
        self.outbound = Some(s);
        self
    }

    pub fn metrics_flush_interval(mut self, dur: Duration) -> Self {
        self.metrics_flush_interval = Some(dur);
        self
    }

    pub fn timer<I>(self, timer: I) -> Proxy<I> {
        Proxy {
            controller: self.controller,
            inbound: self.inbound,
            outbound: self.outbound,
            metrics_flush_interval: self.metrics_flush_interval,
            timer,
        }
    }

    pub fn run(self) -> Listening
    where
        T: Timer + Send + 'static,
        T::Error: Error,
    {
        self.run_with_test_env(config::TestEnv::new())
    }

    pub fn run_with_test_env(self, mut env: config::TestEnv) -> Listening
    where
        T: Timer + Send + 'static,
        T::Error: Error,
    {
        run(self, env)
    }

}

#[derive(Clone, Debug)]
struct MockOriginalDst(Arc<Mutex<DstInner>>);

#[derive(Debug, Default)]
struct DstInner {
    inbound_orig_addr: Option<SocketAddr>,
    inbound_local_addr: Option<SocketAddr>,
    outbound_orig_addr: Option<SocketAddr>,
    outbound_local_addr: Option<SocketAddr>,
}

impl conduit_proxy::GetOriginalDst for MockOriginalDst {
    fn get_original_dst(&self, sock: &TcpStream) -> Option<SocketAddr> {
        sock.local_addr()
            .ok()
            .and_then(|local| {
                let inner = self.0.lock().unwrap();
                if inner.inbound_local_addr == Some(local) {
                    inner.inbound_orig_addr
                } else if inner.outbound_local_addr == Some(local) {
                    inner.outbound_orig_addr
                } else {
                    None
                }
            })
    }
}


fn run<T>(proxy: Proxy<T>, mut env: config::TestEnv) -> Listening
where
    T: Timer + Send + 'static,
    T::Error: Error,
{
    use self::conduit_proxy::config;

    let controller = proxy.controller.expect("proxy controller missing");
    let inbound = proxy.inbound;
    let outbound = proxy.outbound;
    let mut mock_orig_dst = DstInner::default();

    env.put(config::ENV_CONTROL_URL, format!("tcp://{}", controller.addr));
    env.put(config::ENV_PRIVATE_LISTENER, "tcp://127.0.0.1:0".to_owned());
    if let Some(ref inbound) = inbound {
        env.put(config::ENV_PRIVATE_FORWARD, format!("tcp://{}", inbound.addr));
        mock_orig_dst.inbound_orig_addr = Some(inbound.addr);
    }
    if let Some(ref outbound) = outbound {
        mock_orig_dst.outbound_orig_addr = Some(outbound.addr);
    }
    env.put(config::ENV_PUBLIC_LISTENER, "tcp://127.0.0.1:0".to_owned());
    env.put(config::ENV_CONTROL_LISTENER, "tcp://127.0.0.1:0".to_owned());

    env.put(config::ENV_POD_NAMESPACE, "test".to_owned());
    env.put(config::ENV_POD_ZONE, "cluster.local".to_owned());
    env.put(config::ENV_DESTINATIONS_AUTOCOMPLETE_FQDN, "Kubernetes".to_owned());

    let mut config = config::Config::try_from(&env).unwrap();

    // TODO: We currently can't use `config::ENV_METRICS_FLUSH_INTERVAL_SECS`
    // because we need to be able to set the flush interval to a fraction of a
    // second. We should change config::ENV_METRICS_FLUSH_INTERVAL_SECS so that
    // it can support this.
    if let Some(dur) = proxy.metrics_flush_interval {
        config.metrics_flush_interval = dur;
    }

    let mock_orig_dst = MockOriginalDst(Arc::new(Mutex::new(mock_orig_dst)));

    let main = conduit_proxy::Main::new(
        config,
        mock_orig_dst.clone(),
        proxy.timer
    );

    let control_addr = main.control_addr();
    let inbound_addr = main.inbound_addr();
    let outbound_addr = main.outbound_addr();

    {
        let mut inner = mock_orig_dst.0.lock().unwrap();
        inner.inbound_local_addr = Some(inbound_addr);
        inner.outbound_local_addr = Some(outbound_addr);
    }

    let (running_tx, running_rx) = shutdown_signal();
    let (tx, rx) = shutdown_signal();

    ::std::thread::Builder::new()
        .name("support proxy".into())
        .spawn(move || {
            let _c = controller;
            let _i = inbound;
            let _o = outbound;

            let _ = running_tx.send(());
            main.run_until(rx);
        })
        .unwrap();

    running_rx.wait().unwrap();
    ::std::thread::sleep(::std::time::Duration::from_millis(100));

    Listening {
        control: control_addr,
        inbound: inbound_addr,
        outbound: outbound_addr,
        shutdown: tx,
    }
}
