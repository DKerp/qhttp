use crate::listener::*;
use crate::{Handler, Config, Connection};
use crate::http2::Connection as H2Connection;

use std::sync::Arc;
use std::time::{Duration, Instant};
use std::marker::PhantomData;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;
use tokio::task::spawn;
use tokio::sync::{mpsc, watch, oneshot};
use tokio::signal::unix::{signal, SignalKind};

use http::Extensions;

use generic_pool::SyncPool;



pub(crate) enum ConnectionEvent {
    Open,
    Close,
}



/// Manages all connections and answers all incomming requests with the provided [`Handler`].
pub struct Server {
    config: Arc<Config>,
    listeners: Vec<oneshot::Sender<Arc<Box<dyn Handler + 'static>>>>,
    globals: Arc<Extensions>,
    pool: Arc<SyncPool>,

    listener_events_tx: mpsc::UnboundedSender<ListenerEvent>,
    listener_events_rx: mpsc::UnboundedReceiver<ListenerEvent>,
    connection_events_tx: mpsc::UnboundedSender<ConnectionEvent>,
    connection_events_rx: mpsc::UnboundedReceiver<ConnectionEvent>,

    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl Server {
    pub fn new(globals: Extensions) -> Self {
        Self::new_with_config(globals, Config::default())
    }

    pub fn new_with_config(globals: Extensions, config: Config) -> Self {
        if config.request_header_buf_size>config.request_body_buf_size {
            panic!("The request body buffer MUST be at least as big as the request header buffer!");
        }

        // Prepare the channels for signaling events.
        let (listener_events_tx, listener_events_rx) = mpsc::unbounded_channel();
        let (connection_events_tx, connection_events_rx) = mpsc::unbounded_channel();

        // Prepare the channels for orchastrating the gracefull shutdown.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let config = Arc::new(config);
        let listeners = Vec::with_capacity(8);
        let globals = Arc::new(globals);//Extensions::default();
        let pool = Arc::new(SyncPool::default());

        Self {
            config,
            listeners,
            globals,
            pool,
            listener_events_tx,
            listener_events_rx,
            connection_events_tx,
            connection_events_rx,
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Get read-only access to the global variables.
    pub fn globals(&self) -> &Arc<Extensions> {
        &self.globals
    }

    /// Add a listener. Connections accepted by this listener get served immediately without
    /// any further negotiation.
    ///
    /// Mostly usefull for accepting unencrypted HTTP connections.
    pub fn add_listener<IO, L>(&mut self, listener: L)
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
        L: Listener<Conn = IO> + Sync + 'static,
    {
        let negotiator: Option<NoNegotiator> = None;

        self.add_listener_with_negotiator_inner(listener, negotiator);
    }

    /// Add a listener paired with a negotiator. All connections accepted by the listener will
    /// first get negotiated further by the negotiator before they get served.
    ///
    /// Mostly usefull for accepting encrypted HTTPS connections.
    pub fn add_listener_with_negotiator<IO, C, L, N>(&mut self, listener: L, negotiator: N)
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
        C: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
        L: Listener<Conn = IO> + Sync + 'static,
        N: Negotiator<IO, C> + Sync + 'static,
    {
        let negotiator = Some(negotiator);

        self.add_listener_with_negotiator_inner(listener, negotiator);
    }

    fn add_listener_with_negotiator_inner<IO, C, L, N>(&mut self, listener: L, negotiator: Option<N>)
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
        C: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
        L: Listener<Conn = IO> + Sync + 'static,
        N: Negotiator<IO, C> + Sync + 'static,
    {
        let (tx, rx) = oneshot::channel();

        let negotiator = negotiator.map(|n| Arc::new(n));

        let task = ListenerTask::new(
            listener,
            negotiator,
            Arc::clone(&self.config),
        );

        let globals = Arc::clone(&self.globals);
        let pool = Arc::clone(&self.pool);
        let shutdown = self.shutdown_rx.clone();
        let listener_events = self.listener_events_tx.clone();
        let connection_events = self.connection_events_tx.clone();

        spawn(async move {
            task.run(globals, pool, shutdown, listener_events, connection_events, rx).await;
        });

        self.listeners.push(tx);
    }

    /// Start the server and wait for its completion.
    pub async fn run<H: Handler + 'static>(mut self, handler: H) {
        log::info!("Starting QHTTP server.");

        let handler: Box<dyn Handler + 'static> = Box::new(handler);
        let handler = Arc::new(handler);

        // Prepare the channels for signaling events.
        let listener_events_rx = self.listener_events_rx;//mpsc::unbounded_channel();
        let connection_events_rx = self.connection_events_rx;//mpsc::unbounded_channel();

        // Prepare the channels for orchastrating the gracefull shutdown.
        let (shutdown_tx, shutdown_rx) = (self.shutdown_tx, self.shutdown_rx);//watch::channel(false);
        let (shutdown_complete_tx, shutdown_complete_rx) = oneshot::channel();

        // Start the background counter task.
        let counter_task = CounterTask::new(
            listener_events_rx,
            connection_events_rx,
            shutdown_rx.clone(),
            shutdown_complete_tx,
        );
        spawn(async move {
            counter_task.run().await;
        });

        // Start all the listener tasks.
        for task in self.listeners.drain(..) {
            let handler = Arc::clone(&handler);

            task.send(handler).unwrap_or(());
        }

        // Prepare the signal listeners.
        let mut interrupt = signal(SignalKind::interrupt()).unwrap();
        let mut terminate = signal(SignalKind::terminate()).unwrap();

        // Listen for the any signal indicating we should shutdown the server.
        let gracefull = tokio::select! {
            _ = interrupt.recv() => {
                log::info!("Received an SIGINT. Initializing gracefull shutdown.");

                true
            }
            _ = terminate.recv() => {
                log::info!("Received an SIGTERM. Exiting immidiatly.");

                false
            }
        };

        // If a gracefull shutdown was requested, perform it.
        if gracefull {
            // Inform all other important tasks of the gracefull shutdown.
            shutdown_tx.send(true).unwrap();

            log::info!("Will wait at most {:?}.", self.config.gracefull_shutdown_timeout);

            tokio::select! {
                _ = shutdown_complete_rx => {
                    log::info!("Gracefull shutdown completed successfully.");
                }
                _ = tokio::time::sleep(self.config.gracefull_shutdown_timeout) => {
                    log::info!("Gracefull shutdown timeout reached. Exiting immidiatly.");
                }
                _ = interrupt.recv() => {
                    log::info!("Received another SIGINT. Aborting gracefull shutdown. Exiting immidiatly.");
                }
                _ = terminate.recv() => {
                    log::info!("Received an SIGTERM. Exiting immidiatly.");
                }
            }
        }

        log::info!("Shutting down QHTTP server... Bye!");
    }
}


struct CounterTask {
    listener_events: mpsc::UnboundedReceiver<ListenerEvent>,
    listener_events_active: bool,
    listener_counter: u128,

    connection_events: mpsc::UnboundedReceiver<ConnectionEvent>,
    connection_events_active: bool,
    connection_counter: u128,

    shutdown: watch::Receiver<bool>,
    shutdown_complete: oneshot::Sender<()>,
}

impl CounterTask {
    fn new(
        listener_events: mpsc::UnboundedReceiver<ListenerEvent>,
        connection_events: mpsc::UnboundedReceiver<ConnectionEvent>,
        shutdown: watch::Receiver<bool>,
        shutdown_complete: oneshot::Sender<()>,
    ) -> Self {
        let listener_events_active = true;
        let listener_counter = 0;

        let connection_events_active = true;
        let connection_counter = 0;

        Self {
            listener_events,
            listener_events_active,
            listener_counter,

            connection_events,
            connection_events_active,
            connection_counter,

            shutdown,
            shutdown_complete,
        }
    }

    async fn run(mut self) {
        let mut shutdown = false;

        loop {
            tokio::select!{
                event = self.listener_events.recv(), if self.listener_events_active => {
                    if let Some(event) = event {
                        match event {
                            ListenerEvent::Open => self.listener_counter = self.listener_counter.checked_add(1).unwrap_or(u128::MAX),
                            ListenerEvent::Close => self.listener_counter = self.listener_counter.checked_sub(1).unwrap_or(0),
                        };

                        log::debug!("Counter task: The counters changed. listeners: {}, connections: {}", self.listener_counter, self.connection_counter);
                    } else {
                        self.listener_events_active = false;
                    }
                }
                event = self.connection_events.recv(), if self.connection_events_active => {
                    if let Some(event) = event {
                        match event {
                            ConnectionEvent::Open => self.connection_counter = self.connection_counter.checked_add(1).unwrap_or(u128::MAX),
                            ConnectionEvent::Close => self.connection_counter = self.connection_counter.checked_sub(1).unwrap_or(0),
                        }

                        log::debug!("Counter task: The counters changed. listeners: {}, connections: {}", self.listener_counter, self.connection_counter);
                    } else {
                        self.connection_events_active = false;
                    }
                }
                _ = self.shutdown.changed() => {
                    shutdown = true;
                }
            }

            if shutdown {
                if self.listener_counter==0 && self.connection_counter==0 {
                    self.shutdown_complete.send(()).unwrap();
                    return
                }
            }
        }
    }
}


enum ListenerEvent {
    Open,
    Close,
}

struct ListenerTask<IO, C, L, N>
{
    listener: L,
    negotiator: Option<Arc<N>>,
    config: Arc<Config>,
    __io: Option<PhantomData<IO>>,
    __conn: Option<PhantomData<C>>,
}

struct NegotiatorTask<IO, C, N>
{
    negotiator: Arc<N>,
    config: Arc<Config>,
    handler: Arc<Box<dyn Handler + 'static>>,
    __io: Option<PhantomData<IO>>,
    __conn: Option<PhantomData<C>>,
}


impl<IO, C, L, N> ListenerTask<IO, C, L, N>
where
    IO: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    C: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    L: Listener<Conn = IO> + Sync,
    N: Negotiator<IO, C> + Sync + 'static,
{
    fn new(
        listener: L,
        negotiator: Option<Arc<N>>,
        config: Arc<Config>,
    ) -> Self {
        Self {
            listener,
            negotiator,
            config,
            __io: None,
            __conn: None,
        }
    }

    async fn run(
        self,
        globals: Arc<Extensions>,
        pool: Arc<SyncPool>,
        mut shutdown: watch::Receiver<bool>,
        listener_events: mpsc::UnboundedSender<ListenerEvent>,
        connection_events: mpsc::UnboundedSender<ConnectionEvent>,
        start_rx: oneshot::Receiver<Arc<Box<dyn Handler + 'static>>>,
    ) {
        let handler = match start_rx.await {
            Ok(handler) => handler,
            Err(_) => return,
        };

        let connections_per_second = self.config.connections_per_second;
        let mut start = Instant::now();
        let mut count = 0u64;
        let interval = Duration::from_secs(1);

        listener_events.send(ListenerEvent::Open).unwrap_or(());

        loop {
            if connections_per_second>0 {
                log::debug!("Listener.run_inner: count: {}, connections_per_second: {}", count, connections_per_second);
                if count>=connections_per_second {
                    count = 0;
                    let elapsed = start.elapsed();
                    start = Instant::now();

                    if interval>elapsed {
                        let dur = interval-elapsed;
                        log::debug!("Listener.run_inner: Sleeping for {:?} due to too many connections!", dur);
                        tokio::time::sleep(dur).await;
                    }
                }
            }

            let (conn, ext) = tokio::select!{
                result = Listener::accept(&self.listener) => {
                    match result {
                        Ok(pair) => pair,
                        Err(err) => {
                            log::error!("Listener returned error: {:?}", err);
                            break;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    log::info!("ListenerTask.run: Detected gracefull shutdown. Closing listener.");
                    break;
                }
            };

            if connections_per_second>0 {
                count = count+1;
            }

            let config = Arc::clone(&self.config);
            let handler = Arc::clone(&handler);
            let globals = Arc::clone(&globals);
            let pool = Arc::clone(&pool);
            let shutdown = shutdown.clone();
            let connection_events = connection_events.clone();

            if let Some(negotiator) = &self.negotiator {
                let negotiator = Arc::clone(negotiator);

                let negotiator_task = NegotiatorTask::new(negotiator, config, handler);

                spawn(async move {
                    negotiator_task.run(conn, ext, globals, pool, shutdown, connection_events).await;
                });
            } else {
                let connection = Connection::new(
                    config,
                    pool,
                    shutdown,
                    connection_events,
                    conn,
                    ext,
                    globals,
                );

                spawn(async move {
                    connection.handle(handler).await;
                });
            }
        }

        listener_events.send(ListenerEvent::Close).unwrap_or(());
    }
}

impl<IO, C, N> NegotiatorTask<IO, C, N>
where
    IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    C: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    N: Negotiator<IO, C> + Sync,
{
    fn new(
        negotiator: Arc<N>,
        config: Arc<Config>,
        handler: Arc<Box<dyn Handler + 'static>>,
    ) -> Self {
        Self {
            negotiator,
            config,
            handler,
            __io: None,
            __conn: None,
        }
    }

    async fn run(
        self,
        conn: IO,
        mut ext: Extensions,
        globals: Arc<Extensions>,
        pool: Arc<SyncPool>,
        shutdown: watch::Receiver<bool>,
        connection_events: mpsc::UnboundedSender<ConnectionEvent>,
    ) {
        let conn = match timeout(self.config.negotiation_timeout, Negotiator::negotiate(self.negotiator.as_ref(), conn, &mut ext)).await {
            Ok(result) => {
                match result {
                    Ok(conn) => conn,
                    Err(err) => {
                        log::error!("Negotiation failed! err: {:?}", err);
                        return
                    }
                }
            }
            Err(_) => {
                log::debug!("Negotiaton timeout reached. Closing connection. timeout: {:?}", self.config.negotiation_timeout);
                return;
            }
        };

        let config = self.config;
        let handler = self.handler;

        if let Some(info) = ext.get::<ConnectionInfo>() {
            log::info!("Extension: {:?}", info);

            if info.is_h2() {
                let connection = H2Connection::new(
                    config,
                    pool,
                    shutdown,
                    connection_events,
                    conn,
                    ext,
                    globals,
                );

                spawn(async move {
                    connection.handle(handler).await;
                });

                return;
            }
        }

        connection_events.send(ConnectionEvent::Close).unwrap_or(());

        // let connection = Connection::new(
        //     config,
        //     pool,
        //     shutdown,
        //     connection_events,
        //     conn,
        //     ext,
        //     globals,
        // );
        //
        // spawn(async move {
        //     connection.handle(handler).await;
        // });
    }
}
