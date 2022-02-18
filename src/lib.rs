//! The Quantum HTTP (QHTTP) server framework is dedicated to provide a fully functionional HTTP server which enables you to configure all parts of it,
//! which is key to provide a safe and thus production ready web server.
//!
//! This crate is a __work in progress__.


use tokio::io::{AsyncRead, AsyncWrite, BufWriter, ReadBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio::sync::{mpsc, watch, oneshot};

use std::time::{Duration, Instant};
use std::sync::Arc;
use std::future::Future;
use std::pin::Pin;
use std::marker::{Unpin, PhantomData};
use std::task::{Context, Poll};
use std::io::{Error, ErrorKind};
use std::fmt;

use async_trait::async_trait;

use http::Extensions;
use http::header::*;
use http::{StatusCode, HeaderValue};

use chrono::prelude::*;

use generic_pool::SyncPool;



mod parse;
use parse::*;

/// Contains usefull objects which can be used in request handlers.
pub mod util;

/// Contains request related objects.
mod request;
pub use request::HttpRequest;
use request::{ParseHttpRequest, BodyType, LimitReader, ChunkedReader};
use request::{Globals, Locals, Pool};

/// Contains response related objects.
mod response;

mod body;
pub use body::Body;

/// Contains the implementation of the HTTP/2 protocol.
#[allow(dead_code)]
pub mod http2;
use http2::settings::Settings as H2Settings;

mod server;
pub use server::*;

mod listener;
pub use listener::*;

mod buffer;
use buffer::*;

/// Includes all common types which you will need when using this crate.
pub mod prelude {
    pub use crate::{Handler, Config, HttpError, Body, make_handler_fn};
    pub use crate::request::HttpRequest;
    pub use http::Extensions;
    pub use crate::server::Server;
    pub use crate::listener::ConnectionInfo;
}

mod sealed {
    pub trait Sealed {}

    impl<T> Sealed for http::Request<T> {}
}



type BoxError = Box<dyn std::error::Error + Send + Sync>;




/// The trait that request handlers must implement.
#[async_trait]
pub trait Handler: Send + Sync {
    async fn handle(&self, req: http::Request<Body>) -> Result<http::Response<Body>, HttpError>;
}

#[async_trait]
impl<T: Handler> Handler for Box<T> {
    async fn handle(&self, req: http::Request<Body>) -> Result<http::Response<Body>, HttpError> {
        self.handle(req).await
    }
}

#[async_trait]
impl<T: Handler> Handler for Arc<T> {
    async fn handle(&self, req: http::Request<Body>) -> Result<http::Response<Body>, HttpError> {
        self.handle(req).await
    }
}

#[async_trait]
impl<T: Handler> Handler for &T {
    async fn handle(&self, req: http::Request<Body>) -> Result<http::Response<Body>, HttpError> {
        (*self).handle(req).await
    }
}

#[async_trait]
impl<T: Handler> Handler for &mut T {
    async fn handle(&self, req: http::Request<Body>) -> Result<http::Response<Body>, HttpError> {
        (*self).handle(req).await
    }
}


/// Create a handler out of an async function.
pub fn make_handler_fn<F, Fut>(f: F) -> impl Handler//MakeHandlerFn<F, Fut>
where
    F: Fn(http::Request<Body>) -> Fut + Send + Sync,
    Fut: Future<Output = Result<http::Response<Body>, HttpError>> + Send + Sync,
{
    let marker = PhantomData;

    MakeHandlerFn{f, marker}
}

/// The output of [`make_handler_fn`]. Implements the [`Handler`] trait.
struct MakeHandlerFn<F, Fut>
where
    F: Fn(http::Request<Body>) -> Fut + Send + Sync,
    Fut: Future<Output = Result<http::Response<Body>, HttpError>> + Send + Sync,
{
    f: F,
    marker: PhantomData<Fut>,
}

#[async_trait]
impl<F, Fut> Handler for MakeHandlerFn<F, Fut>
where
    F: Fn(http::Request<Body>) -> Fut + Send + Sync,
    Fut: Future<Output = Result<http::Response<Body>, HttpError>> + Send + Sync,
{
    async fn handle(&self, req: http::Request<Body>) -> Result<http::Response<Body>, HttpError> {
        (self.f)(req).await
    }
}




#[derive(Debug)]
pub struct HttpError {
    pub status: http::StatusCode,
    pub close: bool,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HttpError(status: {}, close: {})", self.status.as_str(), self.close)
    }
}

impl std::error::Error for HttpError {}

impl From<http::StatusCode> for HttpError {
    fn from(status: http::StatusCode) -> Self {
        Self {
            status,
            close: false,
        }
    }
}

impl From<std::io::Error> for HttpError {
    fn from(err: Error) -> Self {
        let status = match err.kind() {
            ErrorKind::InvalidData => StatusCode::BAD_REQUEST,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        Self {
            status,
            close: false,
        }
    }
}



/// The main configuration for the [`Server`].
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// The size of the buffer where the request header gets stored.
    /// This does also implicitly define the maximum total request header size.
    ///
    /// __Default__: 4096
    pub request_header_buf_size: usize,
    /// The maximum size of a request header line.
    ///
    /// __Default__: 128
    pub request_header_line_max: usize,
    /// The size of the buffer where the request body gets stored.
    /// This buffer is only used as an intermediate and does not limit the total body size.
    ///
    /// __Default__: 4096
    pub request_body_buf_size: usize,
    /// The size of the buffer where a requests chunked bodys chunk header gets stored.
    /// This buffer is only used as an intermediate to store and parse a single chunks header.
    /// It does neither limit the size of a single chunk, nor does it limit how much can be read
    /// in a single call of [`AsyncRead::poll_read`]. The latter gest limited by the
    /// `request_body_buf_size` value.
    ///
    /// __Default__: 512
    pub request_chunked_header_buf_size: usize,

    /// The maximum number of new connections accepted per second per listener.
    ///
    /// __Default__: 128
    pub connections_per_second: u64,
    /// The maximum duration to wait between the opening of a socket and the finishing of the
    /// (TLS) negotiation.
    ///
    /// __Default__: 10 seconds
    pub negotiation_timeout: Duration,
    /// The maximum duration to wait between the finished establishment of a connection (including negotiation)
    /// and the receiving of the beginning of the first request.
    ///
    /// __Default__: 5 seconds
    pub first_request_timeout: Duration,
    /// The maximum duration to wait between the end of one request (including sending back the response)
    /// and receiving the start of another request.
    ///
    /// __Default__: 60 seconds
    pub idle_timeout: Duration,
    /// The maximum duration to wait between the arrival of the first request header byte(s) and the
    /// arrival of the full request header.
    ///
    /// __Default__: 5 seconds
    pub request_header_timeout: Duration,
    /// The maximum duration to wait for an intermediate "Continue" response line to successfully flush.
    ///
    /// __Default__: 5 seconds
    pub continue_write_timeout: Duration,
    /// The maximum duration to wait for a single chunk of a request body data to arive.
    ///
    /// __Default__: 5 seconds
    pub request_body_read_chunk_timeout: Duration,
    /// The maximum amount of time to wait for the response headers to successfully flush.
    ///
    /// __Default__: 5 seconds
    pub headers_write_timeout: Duration,
    /// The maximum amount of time to wait for the response body to successfully flush.
    ///
    /// __Default__: 300 seconds
    pub body_write_timeout: Duration,
    /// The maximum amount of time to wait for the request handler to complete.
    /// Can be used as a safety procedure in case you can not guarantee that your handler will
    /// eventually complete.
    /// Note that this time does only include the time preparing the response, but NOT sending it.
    ///
    /// __Default__: 60 seconds
    pub handler_timeout: Duration,
    /// The maximum time to wait for all connections to gracefully close by first finishing their current
    /// request/response cicle before performing a forced exit.
    ///
    /// __Default__: 60 seconds
    pub gracefull_shutdown_timeout: Duration,

    /// The size of the [BufWriter] used for sending any data back to the client.
    ///
    /// __Default__: 4096
    pub response_buf_size: usize,
    /// The size of the buffer to use for the response body. The body can be much bigger than this
    /// value.
    ///
    /// __Default__: 4096
    pub response_body_buf_size: usize,
    /// The size of the buffer for a chunked response body. Will correspond to the size of each chunk,
    /// excluding the last one.
    /// Note that the chunks will have 2 bytes less, since the buffer also needs to save the succeeding
    /// CRLF after each chunk body.
    ///
    /// __Default__: 65536
    pub response_chunked_buf_size: usize,

    /// The HTTP/2 specific settings, corresponding to the ones in the RFC.
    pub h2_settings: H2Settings,
    /// The interval at which the server will check if any relevant timeout has been reached.
    ///
    /// __Default__: 1 second
    pub h2_timeouts_check_interval: Duration,
    /// For HTTP/2 connections, the interval at wich a connections write buffer gets flushed.
    /// A flush will happen more often if much data gets written, and a flush will be skipped if
    /// there is no data buffered to be send.
    ///
    /// __Default__: 10 milliseconds (0.01 second)
    pub h2_writer_flush_interval: Duration,
    /// The time to wait for the acknowledgement of a send HTTP/2 settings frame.
    /// After this timeout the connection gets closed with connection error of type settings timeout.
    ///
    /// The server will only check at the interval specified in `h2_timeouts_check_interval` if
    /// this time has passed.
    ///
    /// __Default__: 5 seconds
    pub h2_settings_timeout: Duration,
    /// The time after the last frame got received at a HTTP/2 connection at which the next
    /// HTTP/2 ping should be send.
    ///
    /// The server will only check at the interval specified in `h2_timeouts_check_interval` if
    /// this time has passed.
    ///
    /// __Default__: 60 seconds
    pub h2_ping_interval: Duration,
    /// The time to wait for the acknowledgement of a send HTTP/2 ping frame.
    /// After this timeout the connection gets closed with connection error of type protocol error.
    ///
    /// The server will only check at the interval specified in `h2_timeouts_check_interval` if
    /// this time has passed.
    ///
    /// __Default__: 5 seconds
    pub h2_ping_timeout: Duration,
    /// The payload of the HTTP/2 ping frame. Does not have any significant seurity value.
    ///
    /// __Default__: 42
    pub h2_ping_payload: u64,
    /// The initial global window size. The server will send a corresponding window update frame
    /// at the beginning of an HTTP/2 connection to inform the peer if this value is higher then
    /// the default of 65535 as mandated by the RFC.
    ///
    /// The minium value for this is the default 65535, and the maximum is [`i32::MAX`].
    ///
    /// __Default__: 65535
    pub h2_initial_global_window_size: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            request_header_buf_size: 4096,
            request_header_line_max: 128,
            request_body_buf_size: 4096,
            request_chunked_header_buf_size: 512,

            connections_per_second: 128,
            negotiation_timeout: Duration::from_secs(10),
            first_request_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(60),
            request_header_timeout: Duration::from_secs(5),
            continue_write_timeout: Duration::from_secs(5),
            request_body_read_chunk_timeout: Duration::from_secs(5),
            headers_write_timeout: Duration::from_secs(5),
            body_write_timeout: Duration::from_secs(300),
            handler_timeout: Duration::from_secs(60),
            gracefull_shutdown_timeout: Duration::from_secs(60),

            response_buf_size: 4096,
            response_body_buf_size: 4096,
            response_chunked_buf_size: 65536,

            h2_settings: H2Settings::default(),
            h2_timeouts_check_interval: Duration::from_secs(1),
            h2_writer_flush_interval: Duration::from_millis(10),
            h2_settings_timeout: Duration::from_secs(5),
            h2_ping_interval: Duration::from_secs(60),
            h2_ping_timeout: Duration::from_secs(5),
            h2_ping_payload: 42,
            h2_initial_global_window_size: 65535,
        }
    }
}


struct ReqBodyBuf(Vec<u8>);
struct RespBodyBuf(Vec<u8>);

struct ReqHeaderBuffer(Buffer);


pub(crate) struct Connection {
    config: Arc<Config>,
    pool: Arc<SyncPool>,
    shutdown: watch::Receiver<bool>,
    events: mpsc::UnboundedSender<ConnectionEvent>,
    reader: Box<dyn AsyncRead + Unpin + Send + Sync + 'static>,
    writer: tokio::io::BufWriter<Box<dyn tokio::io::AsyncWrite + Send + Sync + Unpin>>,
    buffer: Buffer,
    resp_body_buf: Vec<u8>,
    ext: Arc<Extensions>,
    globals: Arc<Extensions>,
    first_request: bool,
}

impl Connection {
    pub fn new<C>(
        config: Arc<Config>,
        pool: Arc<SyncPool>,
        shutdown: watch::Receiver<bool>,
        events: mpsc::UnboundedSender<ConnectionEvent>,
        conn: C,
        ext: Extensions,
        globals: Arc<Extensions>,
    ) -> Self
    where
        C: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    {
        let (reader, writer) = tokio::io::split(conn);
        let reader: Box<dyn AsyncRead + Unpin + Send + Sync + 'static> = Box::new(reader);
        let writer: Box<dyn AsyncWrite + Unpin + Send + Sync + 'static> = Box::new(writer);
        let writer = BufWriter::with_capacity(config.response_buf_size, writer);


        let buffer = match pool.get::<ReqHeaderBuffer>() {
            Some(buffer) => {
                log::info!("Received a buffer from the pool!");
                let mut buffer = buffer.0;
                buffer.set_capacity(config.request_header_buf_size);

                buffer
            }
            None => Buffer::with_capacity(config.request_header_buf_size),
        };

        let resp_body_buf = match pool.get::<RespBodyBuf>() {
            Some(body_buf) => {
                log::info!("Received a body buf from the pool!");

                body_buf.0
            }
            None => vec![0u8; config.response_body_buf_size],
        };

        let ext = Arc::new(ext);

        Self {
            config,
            pool,
            shutdown,
            events,
            reader,
            writer,
            buffer,
            resp_body_buf,
            ext,
            globals,
            first_request: true,
        }
    }

    pub async fn handle(
        mut self,
        handler: Arc<Box<dyn Handler + 'static>>,
    ) {
        log::debug!("Serving a new connection!");

        /* Inform the main task of the opened connection */
        self.events.send(ConnectionEvent::Open).unwrap_or(());

        /* Create the channel for sending the request body, if any */
        let (req_body_tx, mut req_body_rx) = mpsc::unbounded_channel();

        loop {
            /* Peek into the next request */
            let mut shutdown = self.shutdown.clone(); // To avoid two mut references...
            tokio::select! {
                // Check if we have a gracefull shutdown running.
                _ = shutdown.changed() => {
                    log::info!("Connection.peek_request: Detected gracefull shutdown. Closing connection.");
                    break;
                }
                result = self.peek_request() => {
                    if let Err(_) = result {
                        break;
                    }
                }
            }

            if self.buffer.available_for_read()==0 {
                log::info!("Connection got regularely reset by the peer. Closing our side.");
                break;
            }

            log::debug!("We found a new request. Starting to parse...");

            /* Prepare benchmarks */
            let now_total = Instant::now();
            let now = Instant::now();

            /* Read and parse the full request header. */
            let req = match self.read_request().await {
                Ok(req) => req,
                Err(_) => break,
            };

            /* Request debugging */
            let path = req.uri().clone();
            log::debug!("Read request time: {:?}, {:?}", path, now.elapsed().as_micros());

            /* Continue parsing and validating the request */
            let body_type = match req.determine_body_type() {
                Ok(body_type) => body_type,
                Err(err) => {
                    log::debug!("Error while determining body type. err: {:?}, req: {:?}", err, req);
                    break;
                }
            };
            if let Err(err) = req.validate() {
                log::debug!("Determined an invalid request. err: {:?}, req: {:?}", err, req);
                break;
            }

            /* Add the costum body */
            let body = match body_type {
                BodyType::NoBody => Body::default(),
                body_type => {
                    let size = if let BodyType::Fixed(size) = body_type {
                        Some(size)
                    } else {
                        None
                    };

                    // Retrieve a buffer for the request body
                    let req_bod_buf: Buffer = match self.pool.get::<ReqBodyBuf>() {
                        Some(body_buf) => body_buf.0.into(),
                        None => vec![0u8; self.config.request_body_buf_size].into(),
                    };

                    let background_read = BackgroundRead::new(
                        req_bod_buf,
                        Arc::clone(&self.pool),
                        req_body_tx.clone(),
                    );

                    let body: Box<dyn AsyncRead + Unpin + Send + Sync + 'static> = match size {
                        Some(size) => Box::new(LimitReader::new(background_read, size)),
                        None => Box::new(ChunkedReader::new(background_read, self.config.request_chunked_header_buf_size, Arc::clone(&self.pool))),
                    };

                    Body {
                        body: Some(body),
                        body_len: size,
                        finish_on_drop: true,
                    }
                }
            };
            let mut req = req.map(move |_| body);

            /* Setting standard headers */
            // resp.set_header_bytes(HeaderKey::Connection, b"keep-alive").unwrap();
            //resp.set_header_bytes(HeaderKey::Server, b"QHTTP").unwrap();

            /* Add additional extensions */
            let extensions = req.extensions_mut();
            extensions.insert(Globals::from(Arc::clone(&self.globals)));
            extensions.insert(Locals::from(Arc::clone(&self.ext)));
            extensions.insert(Pool::from(Arc::clone(&self.pool)));

            /* Preperation of the request is done */
            let req = req; // Remove mutability

            let mut expects_continue = req.expects_continue();
            let version = req.version();

            /* Execute the user supplied handler in a seperate task */
            let duration = self.config.handler_timeout;
            let handler = Arc::clone(&handler);

            let now = Instant::now();
            let mut task = tokio::spawn(async move {
                timeout(duration, handler.handle(req)).await
            });

            /* While executing the handler, respond to read request from the request body */
            let result = loop {
                tokio::select! {
                    result = &mut task => break Some(result),
                    read_req = req_body_rx.recv() => {
                        // Receiving can not fail because we keep a sender half.
                        let (buffer, amount, tx) = read_req.unwrap();

                        if let Err(_) = self.handle_background_read_req(
                            &mut expects_continue,
                            version,
                            buffer,
                            amount,
                            tx,
                        ).await {
                            break None;
                        }
                    }
                }
            };

            log::info!("Handle total time: {:?}, {:?}", path, now.elapsed());

            let result = match result {
                Some(result) => result,
                None => break,
            };

            /* Ensure the task did not panic */
            let result = match result {
                Ok(result) => result,
                Err(err) => {
                    log::error!("Request handler task paniced! err: {:?}", err);
                    break;
                }
            };

            /* Evaluate the result of the handle task, extracting the resp if any */
            let mut resp = match result {
                Ok(handle_result) => {
                    match handle_result {
                        Ok(resp) => resp,
                        Err(err) => {
                            log::error!("Error while handling request: {:?}", err);

                            let mut resp = http::Response::new(Body::default());

                            *resp.status_mut() = err.status;
                            resp.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
                            break;
                        }
                    }
                }
                Err(_) => {
                    log::warn!("Request handler timed out!");
                    break;
                }
            };

            /* Add the date header if necessary */
            if let None = resp.headers().get(DATE) {
                let date = Utc::now().to_rfc2822();
                let date = String::from(&date[0..(date.len()-5)])+"GMT";
                let date = HeaderValue::from_bytes(date.as_bytes()).unwrap();

                resp.headers_mut().insert(DATE, date);
            }

            /* Send the response */
            let now = Instant::now();
            if let Err(err) = self.write_response(resp).await {
                log::warn!("Writing response failed! err: {:?}", err);
                break;
            }

            /* Log the benchmarks */
            log::info!("Write response total time: {:?}, {:?}", path, now.elapsed());
            log::info!("Handle total time: {:?}, {:?}", path, now_total.elapsed());
        };

        /* Recycle objects for later reuse */
        self.pool.put(ReqHeaderBuffer(self.buffer));

        /* Inform the main task of the closed connection */
        self.events.send(ConnectionEvent::Close).unwrap_or(());

        log::debug!("Closing socket...");
    }

    async fn peek_request(&mut self) -> Result<(), ()> {
        if self.buffer.available_for_read()>0 {
            // There is already an overhead from the last request belonging to the next(current) request.
            return Ok(())
        }

        let duration = if self.first_request {
            self.first_request = false;

            self.config.first_request_timeout
        } else {
            self.config.idle_timeout
        };

        match timeout(duration, self.peek_request_inner()).await {
            Ok(result) => {
                if let Err(err) = result {
                    log::error!("Error while peeking into next request. Aborting. err: {:?}", err);
                    return Err(());
                }
            },
            Err(_) => {
                log::debug!("Request idle timeout reached. Closing connection.");
                return Err(());
            }
        };

        Ok(())
    }

    async fn peek_request_inner(&mut self) -> Result<(), ()> {
        match self.read_into_buffer().await {
            Ok(_) => return Ok(()),
            Err(err) => {
                log::error!("Reading into the buffer failed while peeking into next request. err: {:?}", err);

                return Err(());
            }
        }
    }

    async fn read_request(&mut self) -> Result<http::Request<()>, ()> {
        match timeout(self.config.request_header_timeout, self.read_request_inner()).await {
            Ok(result) => {
                match result {
                    Ok(req) => return Ok(req),
                    Err(err) => {
                        log::error!("Error while reading next request. Aborting. err: {:?}", err);
                        log::debug!("Buffer content: {}", String::from_utf8_lossy(self.buffer.as_ref()));
                        log::debug!("Buffer content raw: {:?}", self.buffer.as_ref());
                        return Err(());
                    }
                }
            },
            Err(_) => {
                log::debug!("Request read request header timeout reached. Closing connection.");
                return Err(());
            }
        }
    }

    async fn read_request_inner(&mut self) -> Result<http::Request<()>, BoxError> {

        // Read the start line.
        let idx = loop {
            match find_start_line_end(self.buffer.as_ref())? {
                Some(idx) => break idx,
                None => {
                    if self.read_into_buffer().await?==0 {
                        return Err(Error::new(ErrorKind::UnexpectedEof, "No full request header start line was send.").into());
                    }
                }
            };
        };

        // Parse the request basics.
        let (method, uri, version) = parse_start_line2(&self.buffer.as_ref()[..idx])?;
        let mut req = http::Request::new(());
        *req.method_mut() = method;
        *req.uri_mut() = uri;
        *req.version_mut() = version;

        self.buffer.progress(idx);

        // Read some more data if necessary.
        if self.buffer.available_for_read()==0 {
            if self.read_into_buffer().await?==0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "No request header was send.").into());
            }
        }

        // Read all header lines.
        loop {
            let buffer = self.buffer.as_ref();

            if buffer[0]==CR {
                log::debug!("We found a CR...");
                if let Some(&b) = buffer.get(1) {
                    if b==NL {
                        // We found the ending CRNL.
                        self.buffer.progress(2);
                        break;
                    } else {
                        return Err(Error::new(ErrorKind::InvalidData, "A header line started with a CR but was not followed by NL.").into());
                    }
                }
            } else {
                if let Some(idx) = find_header_end(buffer)? {
                    log::debug!("We found a request header.");

                    if idx>self.config.request_header_line_max {
                        return Err(Error::new(ErrorKind::InvalidData, "A header line exceeded the limit.").into());
                    }

                    // NOTE find_header_end will ensure there is another byte after the end of the header.
                    // We send it over again to enable the parse function to determine the end again.
                    let (name, mut values) = parse_request_header_new(buffer)?;

                    // Add the header to the others.
                    let headers = req.headers_mut();
                    for value in values.drain(..) {
                        headers.append(name.clone(), value);
                    }

                    self.buffer.progress(idx);

                    // NOTE find_header_end will only signal the header end if there is a byte after it.
                    continue;
                }
            }

            if self.read_into_buffer().await?==0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "No full request header was send.").into());
            }
        }

        Ok(req)
    }

    async fn handle_background_read_req(
        &mut self,
        expects_continue: &mut bool,
        version: http::Version,
        mut buffer: Buffer,
        mut amount: usize,
        tx: oneshot::Sender<std::io::Result<Buffer>>,
    ) -> Result<(), ()> {
        log::debug!("Connection.handle_background_read_req - buffer: {:?}, amount: {}", buffer, amount);

        buffer.rotate_buffer();
        let available = buffer.available_for_write();
        if amount>available {
            amount = available;
        }

        // Send the continue line if necessary.
        if *expects_continue {
            log::debug!("Sending continue line...");
            *expects_continue = false;

            let duration = self.config.continue_write_timeout;
            match timeout(duration, self.write_continue(version)).await {
                Ok(result) => {
                    if let Err(err) = result {
                        log::debug!("Sending the continue header failed. err: {:?}", err);
                        return Err(());
                    }
                },
                Err(_) => {
                    log::debug!("Sending the continue header timed out.");
                    return Err(());
                }
            };

            log::debug!("Sending continue line successfull.");
        }

        // If we still ahve data in the buffer, read from there.
        let available = self.buffer.available_for_read();
        if available>0 {
            if available>=amount {
                let result = std::io::Write::write(&mut buffer, &self.buffer.as_ref()[..amount]);
                self.buffer.progress(amount);
                let result = result.map(move |_| buffer);

                tx.send(result).unwrap_or(());
                return Ok(());
            } else {
                let n = std::io::Write::write(&mut buffer, &self.buffer.as_ref()).unwrap(); // Can not fail.
                self.buffer.progress(n);
                amount = amount-n;
            }
        }

        // Read a chunk into the internal buffer.
        let duration = self.config.request_body_read_chunk_timeout;
        let result = match timeout(duration, self.read_into_buffer()).await {
            Ok(result) => result,
            Err(_) => {
                log::debug!("Reading a single request body chunk timed out.");
                let result = Err(Error::new(ErrorKind::TimedOut, "Reading into the buffer timed out."));
                tx.send(result).unwrap_or(()); // Give the error back to the handler task.
                return Err(());
            }
        };

        // Handle a possible read error.
        match result {
            Ok(n) => {
                if n==0 {
                    let result = Err(Error::new(ErrorKind::UnexpectedEof, "No full body was send."));
                    tx.send(result).unwrap_or(()); // Give the error back to the handler task.
                    return Err(());
                }
            }
            Err(err) => {
                log::debug!("Reading a single request body chunk failed. err: {:?}", err);
                let result = Err(err);
                tx.send(result).unwrap_or(()); // Give the error back to the handler task.
                return Err(());
            }
        }

        // Write the chunk into the body buffer. We know thre is now data availabe.
        let available = self.buffer.available_for_read();
        if available>=amount {
            std::io::Write::write(&mut buffer, &self.buffer.as_ref()[..amount]).unwrap(); // Can not fail.
            self.buffer.progress(amount);
        } else {
            let n = std::io::Write::write(&mut buffer, &self.buffer.as_ref()).unwrap(); // Can not fail.
            self.buffer.progress(n);
        }

        let result = Ok(buffer);

        // Ignore it if sending fails. Maybe the body got dropped in the meantime.
        tx.send(result).unwrap_or(());

        Ok(())
    }

    async fn write_continue(&mut self, version: http::Version) -> std::io::Result<()> {
        match timeout(self.config.continue_write_timeout, self.write_continue_inner(version)).await {
            Ok(result) => {
                if let Err(err) = result {
                    log::error!("Writing continue failed: {:?}", err);
                    return Err(err);
                }
            }
            Err(_) => {
                log::warn!("Timeout while writing continue status code!");
                return Err(Error::new(ErrorKind::TimedOut, "Writing continue took too long."));
            }
        }

        Ok(())
    }

    async fn write_continue_inner(&mut self, version: http::Version) -> std::io::Result<()> {
        let sp = &[SP];

        // Write status line.
        let version = format!("{:?}", version);
        self.writer.write_all(version.as_bytes()).await?;
        self.writer.write_all(sp).await?;
        let status = http::StatusCode::CONTINUE;
        self.writer.write_all(status.as_str().as_bytes()).await?;
        self.writer.write_all(sp).await?;
        if let Some(reason) = status.canonical_reason() {
            self.writer.write_all(reason.as_bytes()).await?;
        }
        self.writer.write_all(CRNL).await?;

        self.writer.write_all(CRNL).await?;
        self.writer.flush().await?;

        Ok(())
    }

    async fn write_response(&mut self, mut resp: http::Response<Body>) -> Result<(), BoxError> {
        log::info!("Writing response... status: {:?}, body_len: {:?}", resp.status(), resp.body().body_len);

        self.finalize_response(&mut resp);

        let now = Instant::now();

        match timeout(self.config.headers_write_timeout, self.write_headers(&resp)).await {
            Ok(result) => {
                if let Err(err) = result {
                    log::error!("Writing headers failed: {:?}", err);
                    return Err(err.into());
                }
            }
            Err(_) => {
                log::warn!("Timeout while writing response headers!");
                return Err(Error::new(ErrorKind::TimedOut, "Writing headers took too long.").into());
            }
        }

        let headers_dur = now.elapsed();

        let body = resp.into_body();

        let now = Instant::now();

        match timeout(self.config.body_write_timeout, self.write_body(body)).await {
            Ok(result) => {
                if let Err(err) = result {
                    log::error!("Writing body failed: {:?}", err);
                    return Err(err.into());
                }
            }
            Err(_) => {
                log::warn!("Timeout while writing response body!");
                return Err(Error::new(ErrorKind::TimedOut, "Writing body took too long.").into());
            }
        }

        let body_dur = now.elapsed();

        log::info!("Sending headers took {:?}, sending the body took {:?}.", headers_dur, body_dur);

        Ok(())
    }

    fn finalize_response(&self, resp: &mut http::Response<Body>) {
        let body = resp.body();
        if let Some(_) = &body.body {
            match &body.body_len {
                Some(body_len) => {
                    let header = http::HeaderValue::from_bytes(body_len.to_string().as_bytes()).unwrap();
                    resp.headers_mut().insert(http::header::CONTENT_LENGTH, header);
                }
                None => {
                    let header = http::HeaderValue::from_static("chunked");
                    resp.headers_mut().insert(http::header::TRANSFER_ENCODING, header);
                }
            }
        } else {
            resp.headers_mut().insert(http::header::CONTENT_LENGTH, http::HeaderValue::from_static("0"));
        }
    }

    async fn write_headers(
        &mut self,
        resp: &http::Response<Body>,
    ) -> std::io::Result<()> {
        let sp = &[SP];

        // Write status line:
        let version = format!("{:?}", resp.version());
        self.writer.write_all(version.as_bytes()).await?;
        self.writer.write_all(sp).await?;
        let status = resp.status();
        self.writer.write_all(status.as_str().as_bytes()).await?;
        self.writer.write_all(sp).await?;
        if let Some(reason) = status.canonical_reason() {
            self.writer.write_all(reason.as_bytes()).await?;
        }
        self.writer.write_all(CRNL).await?;

        for (key, value) in resp.headers().iter() {
            self.writer.write_all(key.as_ref()).await?;
            self.writer.write_all(b": ").await?;
            self.writer.write_all(value.as_bytes()).await?;
            self.writer.write_all(CRNL).await?;
        }

        self.writer.write_all(CRNL).await?;
        self.writer.flush().await?;

        Ok(())
    }

    async fn write_body(
        &mut self,
        mut own_body: Body,
    ) -> std::io::Result<()> {
        let mut body = match own_body.body.take() {
            Some(body) => body,
            None => return Ok(()),
        };

        if let Some(body_len) = own_body.body_len.take() {
            let mut count = 0usize;

            while body_len>count {
                let n = body.read(&mut self.resp_body_buf[..]).await?;

                self.writer.write_all(&self.resp_body_buf[..n]).await?;

                count = count+n;
            }

            self.writer.flush().await?;
        } else {
            let mut writer = crate::response::ChunkedWriter::new(&mut self.writer, self.config.response_chunked_buf_size);

            loop {
                let n = body.read(&mut self.resp_body_buf[..]).await?;
                if n==0 {
                    // EOF reached. abort.
                    break;
                }

                writer.write_all(&self.resp_body_buf[..n]).await?;
            }

            // Finish the chunked writing.
            writer.shutdown().await?;
        }

        self.writer.flush().await?;

        Ok(())
    }


    async fn read_into_buffer(&mut self) -> std::io::Result<usize> {
        self.buffer.read_from_reader(&mut self.reader).await
    }
}



struct BackgroundRead {
    buffer: Option<Buffer>,
    pool: Arc<SyncPool>,
    sender: mpsc::UnboundedSender<(Buffer, usize, oneshot::Sender<std::io::Result<Buffer>>)>,
    receiver: Option<oneshot::Receiver<std::io::Result<Buffer>>>,
}

impl AsyncRead for BackgroundRead {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let inner = Pin::into_inner(self);

        // First check if we have some unread bytes.
        if let Some(buffer) = inner.buffer.as_mut() {
            let available = buffer.available_for_read();
            if available>0 {
                log::debug!("BackgroundRead - buffer had some unread bytes ready. available: {}", available);
                let amount = if available>buf.remaining() {
                    buf.remaining()
                } else {
                    available
                };

                buf.put_slice(&buffer.as_ref()[..amount]);
                buffer.progress(amount);

                return Poll::Ready(Ok(()));
            }
        }

        // Either we are waiting for a response of the other task, or we have to send
        // a read request to it.
        if let Some(buffer) = inner.buffer.take() {
            log::debug!("BackgroundRead - We had an empty buffer. Sending to the connection task...");
            // Create the answer channel.
            let (tx, mut rx) = oneshot::channel();

            // Send a request to the connection task to read some data.
            if let Err(err) = inner.sender.send((buffer, buf.remaining(), tx)) {
                log::error!("BackgroundRead - could not send buffer to the connection task! err: {:?}", err);
                return Poll::Ready(Err(Error::new(ErrorKind::Other, "Internal error. (Sending read request failed)")));
            }

            // We have to poll the receiver here to ensure the context registers the wakeup.
            let result = match Pin::new(&mut rx).poll(cx) {
                Poll::Ready(result) => result,
                Poll::Pending => {
                    log::debug!("BackgroundRead - There was no immediate result ready. Expected.");
                    // Expected - add the oneshot receiver to the object so we can poll again later.
                    inner.receiver = Some(rx);
                    return Poll::Pending;
                }
            };

            // Extract the buffer, if any.
            let mut buffer = match result {
                Ok(read_result) => {
                    match read_result {
                        Ok(buffer) => buffer,
                        Err(err) => {
                            log::error!("BackgroundRead - reading failed! err: {:?}", err);
                            return Poll::Ready(Err(err));
                        }
                    }
                },
                Err(err) => {
                    log::error!("BackgroundRead - could not receive buffer from the connection task! err: {:?}", err);
                    return Poll::Ready(Err(Error::new(ErrorKind::Other, "Internal error. (Receiving read response failed)")));
                }
            };

            // Read from the buffer and save it afterwards. Send EOF if the buffer has no new bytes.
            let available = buffer.available_for_read();
            if available>0 {
                let amount = if available>buf.remaining() {
                    buf.remaining()
                } else {
                    available
                };

                buf.put_slice(&buffer.as_ref()[..amount]);
                buffer.progress(amount);

                inner.buffer = Some(buffer);
                log::debug!("BackgroundRead - Successfully read immediate read result. available: {}", available);
            }

            inner.receiver = Some(rx);

            // NOTE Will equal to an EOF if we did no fill anything into the poll_read buffer.
            return Poll::Ready(Ok(()))
        }

        // If there is no buffer, we might need to poll for the response.
        if let Some(mut rx) = inner.receiver.take() {
            log::debug!("BackgroundRead - Polling for a response...");
            // Poll the receiver.
            let result = match Pin::new(&mut rx).poll(cx) {
                Poll::Ready(result) => result,
                Poll::Pending => {
                    log::debug!("BackgroundRead - Response was not ready yet.");
                    inner.receiver = Some(rx);
                    return Poll::Pending;
                }
            };

            // Extract the buffer, if any.
            let mut buffer = match result {
                Ok(read_result) => {
                    match read_result {
                        Ok(buffer) => buffer,
                        Err(err) => {
                            log::error!("BackgroundRead - reading failed! err: {:?}", err);
                            return Poll::Ready(Err(err));
                        }
                    }
                },
                Err(err) => {
                    log::error!("BackgroundRead - could not receive buffer from the connection task! err: {:?}", err);
                    return Poll::Ready(Err(Error::new(ErrorKind::Other, "Internal error. (Receiving read response failed)")));
                }
            };

            // Read from the buffer and save it afterwards. Send EOF if the buffer has no new bytes.
            let available = buffer.available_for_read();
            if available>0 {
                let amount = if available>buf.remaining() {
                    buf.remaining()
                } else {
                    available
                };

                buf.put_slice(&buffer.as_ref()[..amount]);
                buffer.progress(amount);

                inner.buffer = Some(buffer);
                inner.receiver = Some(rx);
                log::debug!("BackgroundRead - Successfully read read result. available: {}", available);
            }

            // NOTE Will equal to an EOF if we did no fill anything into the poll_read buffer.
            return Poll::Ready(Ok(()))
        }

        // If neither exists, we are either done (EOF) or there was an error. Abort.
        Poll::Ready(Err(Error::new(ErrorKind::Other, "This object is no longer usable.")))
    }
}

impl BackgroundRead {
    fn new(
        buffer: Buffer,
        pool: Arc<SyncPool>,
        sender: mpsc::UnboundedSender<(Buffer, usize, oneshot::Sender<std::io::Result<Buffer>>)>,
    ) -> Self {
        let buffer = Some(buffer);

        Self {
            buffer,
            pool,
            sender,
            receiver: None,
        }
    }
}

impl Drop for BackgroundRead {
    fn drop(&mut self) {
        if let Some(body_buf) = self.buffer.take() {
            let body_buf = body_buf.into_inner();

            self.pool.put(ReqBodyBuf(body_buf));
        }
    }
}
