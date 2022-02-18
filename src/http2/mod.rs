use crate::{Config, Handler, Body, Globals, Locals, Pool};
use crate::buffer::Buffer;
use crate::server::ConnectionEvent;

use std::convert::{TryFrom, TryInto};
use std::sync::Arc;
use std::marker::Unpin;
use std::io::{Error, ErrorKind};
use std::io::Write;
use std::collections::HashMap;
use std::time::Instant;
use std::ops::Add;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, BufWriter};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio::time::{timeout, interval};

use generic_pool::SyncPool;

use http::Extensions;
use http::header::{HeaderName, HeaderValue};

use hpack::{Decoder, Encoder};

use bitflags::bitflags;

use num_derive::FromPrimitive;
use num_traits::FromPrimitive;



pub mod settings;
use settings::*;

pub mod frame;
use frame::*;

pub mod stream;
use stream::*;

mod error;
pub use error::*;



pub(crate) static HTTP2_CLIENT_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";



#[derive(Debug)]
pub(crate) enum ConnectionReq {
    /// A frame to be send back to the client.
    Frame(Frame),
    /// A response (headers only) to be send back to the client.
    /// Does also include the id of the stream the response belongs to as well
    /// as the end_stream flag.
    Response((http::Response<()>, u32, bool)),
    /// A request to consume the specified amount of the global window.
    GetWindow((u32, i32)), // id, amount
    /// Gives back to the global window in the event of a short read or read error.
    PutWindow(i32), // amount
    /// Increases the window available for the peer, both for the stream as well as
    /// globally.
    UpdatePeerWindow((u32, i32)), // id, amount
    /// A stream switched to the closed state.
    StreamClosing(u32, Option<H2Error>),
    /// A stream closed its task entirely.
    StreamClosed(u32),
}

pub(crate) struct Connection<'a> {
    config: Arc<Config>,
    pool: Arc<SyncPool>,

    shutdown: watch::Receiver<bool>,
    events: mpsc::UnboundedSender<ConnectionEvent>,

    remote_settings: Settings,
    local_settings: Settings,

    last_frame_received: Instant,
    settings_ack_timeout: Option<Instant>,
    ping_ack_timeout: Option<Instant>,

    reader: Box<dyn AsyncRead + Unpin + Send + Sync + 'static>,
    writer: BufWriter<Box<dyn AsyncWrite + Send + Sync + Unpin>>,

    buffer: Buffer,
    header_buffer: Buffer,
    header_decoder: Decoder<'a>,
    header_encoder: Encoder<'a>,

    ext: Arc<Extensions>,
    globals: Arc<Extensions>,

    streams: HashMap<u32, StreamHandle>,
    biggest_open_stream: u32,
    open_streams: u32,
    global_window: i32,
    peer_window: i32,
    pending_window_req: Vec<(u32, i32)>, // id, amount.
    pending_gracefull_shutdown: bool,
}

// struct ConnectionWriter {
//     writer: BufWriter<Box<dyn AsyncWrite + Send + Sync + Unpin>>,
// }

impl<'a> Connection<'a> {
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

        let remote_settings = Settings::default();
        let local_settings = config.h2_settings;

        let last_frame_received = Instant::now();
        let settings_ack_timeout = None;
        let ping_ack_timeout = None;

        let max_frame_size = config.h2_settings.max_frame_size as usize;
        let buffer = match pool.get::<Buffer>() {
            Some(mut buffer) => {
                log::debug!("Received a buffer from the pool!");
                buffer.set_capacity(max_frame_size);

                buffer
            }
            None => Buffer::with_capacity(max_frame_size),
        };

        let header_buffer = match pool.get::<Buffer>() {
            Some(mut buffer) => {
                log::info!("Received a buffer from the pool!");
                buffer.set_capacity(config.request_header_buf_size);

                buffer
            }
            None => Buffer::with_capacity(config.request_header_buf_size),
        };

        let header_decoder = Decoder::new();
        let header_encoder = Encoder::new();

        let ext = Arc::new(ext);

        let streams = HashMap::new();

        let pending_window_req = Vec::with_capacity(100);

        Self {
            config,
            pool,
            shutdown,
            events,
            remote_settings,
            local_settings,
            last_frame_received,
            settings_ack_timeout,
            ping_ack_timeout,
            reader,
            writer,
            buffer,
            header_buffer,
            header_decoder,
            header_encoder,
            ext,
            globals,
            streams,
            biggest_open_stream: 0,
            open_streams: 0,
            global_window: 65_535,
            peer_window: 65_535,
            pending_window_req,
            pending_gracefull_shutdown: false,
        }
    }

    pub async fn handle(
        mut self,
        handler: Arc<Box<dyn Handler + 'static>>,
    ) {
        log::debug!("Serving a new H2 connection!");

        /* Inform the main task of the opened connection */
        self.events.send(ConnectionEvent::Open).unwrap_or(());

        /* Perform the main loop, serving the connection */
        self.handle_inner(handler).await;

        /* Recycle objects for later reuse */
        self.pool.put(self.buffer);

        /* Inform the main task of the closed connection */
        self.events.send(ConnectionEvent::Close).unwrap_or(());

        log::warn!("Closing H2 connection...");
    }

    async fn handle_inner(
        &mut self,
        handler: Arc<Box<dyn Handler + 'static>>,
    ) {
        // Scan for the H2 connection preface.
        if let Err(err) = self.read_preface().await {
            log::error!("Reading H2 connection preface failed. err: {:?}", err);
            return
        }

        // Send the initial settings frame.
        let frame: SettingsFrame = self.local_settings.into();

        log::debug!("Initial settings frame (local): {:?}", frame);

        if let Err(err) = self.write_frame(frame).await {
            log::error!("Writing initial H2 settings frame failed. err: {:?}", err);
            return;
        }

        // Send an initial global window update frame, if necessary.
        self.peer_window = self.config.h2_initial_global_window_size;
        let window_size_increment = match self.config.h2_initial_global_window_size.checked_sub(65535) {
            Some(i) => i,
            None => {
                log::error!("Calculating initital global window size increment failed duo to an underflow!");
                return;
            }
        };
        let window_size_increment: u32 = match window_size_increment.try_into() {
            Ok(i) => i,
            Err(_) => {
                log::error!("Converting the intitial global window size increment to u32 failed!");
                return;
            }
        };
        if window_size_increment>0 {
            let frame = WindowUpdateFrame {
                stream: 0,
                window_size_increment,
            };

            log::debug!("Sending the peer an initial global WindowUpdate frame: {:?}", frame);

            if let Err(err) = self.write_frame(frame).await {
                log::warn!("Writing initial global WindowUpdate frame failed. err: {:?}", err);
                return;
            }
        }

        self.settings_ack_timeout = Some(Instant::now().add(self.config.h2_settings_timeout));

        let error_code = match self.main_loop(handler).await {
            Ok(()) => ErrorCode::NoError,
            Err(err) => {
                log::warn!("DEBUG - Main loop exited with an error: {:?}", err);

                err.into_inner()
            }
        };
        let error_code = Some(error_code);

        // Determine the last stream id.
        let last_stream_id = self.biggest_open_stream;

        // Send a go away frame.
        let frame = GoAwayFrame {
            last_stream_id,
            error_code,
            debug_data: Vec::with_capacity(0),
        };

        log::debug!("Final go away frame: {:?}", frame);

        if let Err(err) = self.write_frame(frame).await {
            log::debug!("Writing final H2 go away frame failed. err: {:?}", err);
        }
    }

    async fn main_loop(
        &mut self,
        handler: Arc<Box<dyn Handler + 'static>>,
    ) -> H2Result<()> {
        /* Create the channel for receiving request from the background stream tasks. */
        let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<ConnectionReq>();

        // Create the interval for sending ping frames.
        let mut timeouts_check_interval = interval(self.config.h2_timeouts_check_interval);

        // Create the interval for flushing the write buffer, if necessary.
        let mut flush_write_buffer_interval = interval(self.config.h2_writer_flush_interval);

        let mut shutdown = self.shutdown.clone(); // To avoid two mut references...

        loop {
            /* Read the next frame */
            let frame = tokio::select! {
                result = self.read_next_frame() => {
                    let frame = result?;

                    self.last_frame_received = Instant::now();

                    frame
                }
                _ = shutdown.changed() => {
                    log::info!("H2 Connection - Detected graceful shutdown.");

                    if self.open_streams==0 {
                        log::info!("H2 gracefull shutdown successfull. (No streams open) Closing connection.");
                        return Ok(())
                    }

                    self.pending_gracefull_shutdown = true;

                    // Inform the remote peer that we do not want new streams to be opened.
                    let mut frame = SettingsFrame::default();
                    frame.max_concurrent_streams = Some(0);

                    self.write_frame(frame).await?;

                    self.settings_ack_timeout = Some(Instant::now().add(self.config.h2_settings_timeout));

                    continue;
                }
                _ = flush_write_buffer_interval.tick() => {
                    if self.writer.buffer().len()>0 {
                        self.writer.flush().await.map_err(|_| H2Error::Connection(ErrorCode::ProtocolError))?;
                    }

                    continue;
                }
                now = timeouts_check_interval.tick() => {
                    let now = now.into_std();

                    if let Some(timeout) = self.settings_ack_timeout {
                        if now>timeout {
                            log::warn!("DEBUG - settings ack timeout reached. Closing connection.");
                            return Err(H2Error::Connection(ErrorCode::SettingsTimeout));
                        }
                    }
                    if let Some(timeout) = self.ping_ack_timeout {
                        if now>timeout {
                            log::warn!("DEBUG - ping ack timeout reached. Closing connection.");
                            return Err(H2Error::Connection(ErrorCode::ProtocolError));
                        }
                    }

                    let timeout = self.last_frame_received.add(self.config.h2_ping_interval);
                    if now>timeout {
                        log::debug!("PING interval reached. Sending ping.");

                        let frame = PingFrame {
                            ack: false,
                            data: self.config.h2_ping_payload,
                        };

                        self.write_frame(frame).await?;

                        self.ping_ack_timeout = Some(now.add(self.config.h2_ping_timeout));
                    }

                    continue;
                }
                conn_req = conn_rx.recv() => {
                    let conn_req = conn_req.unwrap(); // Safe because we maintain a sender half.

                    // log::info!("Received a conn_req: {:?}", conn_req);

                    match conn_req {
                        ConnectionReq::Frame(frame) => {
                            log::debug!("Received a H2 frame to be send. type: {:?}, id: {}", frame.header.r#type, frame.header.stream);
                            self.write_frame(frame).await?;
                        }
                        ConnectionReq::Response((resp, id, end_stream)) => {
                            log::debug!("Received a H2 response to be send. resp: {:?}, id: {}, end_stream: {}", resp, id, end_stream);
                            self.write_response(resp, id, end_stream).await?;
                        }
                        ConnectionReq::GetWindow((id, amount)) => {
                            self.pending_window_req.push((id, amount));

                            self.distribute_window()?;
                        }
                        ConnectionReq::PutWindow(amount) => {
                            // Update the global window.
                            self.update_global_window(amount)?;

                            self.distribute_window()?;
                        }
                        ConnectionReq::UpdatePeerWindow((id, amount)) => {
                            log::debug!("Got an UpdatePeerWindow req. id: {}, amount: {}, peer_window: {}", id, amount, self.peer_window);
                            // Update the peers global window.
                            self.peer_window = match self.peer_window.checked_add(amount) {
                                Some(window) => window,
                                None => {
                                    log::warn!("Updating global peer window failed due to overflow! peer_window: {}, amount: {}", self.peer_window, amount);
                                    return Err(H2Error::Connection(ErrorCode::InternalError));
                                }
                            };

                            let window_size_increment: u32 = match amount.try_into() {
                                Ok(window) => window,
                                Err(_) => {
                                    log::warn!("Generating global peer window update failed due to an invalid amount: {}", amount);
                                    return Err(H2Error::Connection(ErrorCode::InternalError));
                                }
                            };

                            // Inform the peer of the window update.
                            let frame = WindowUpdateFrame {
                                stream: 0,
                                window_size_increment,
                            };

                            log::debug!("Sending the peer a global WindowUpdate frame: {:?}", frame);

                            self.write_frame(frame).await?;

                            let frame = WindowUpdateFrame {
                                stream: id,
                                window_size_increment,
                            };

                            log::debug!("Sending the peer a stream WindowUpdate frame: {:?}", frame);

                            self.write_frame(frame).await?;
                        }
                        ConnectionReq::StreamClosing(id, error) => {
                            if let Some(error) = error {
                                self.handle_possible_stream_error(Err(error), id).await?;
                            }

                            self.open_streams = match self.open_streams.checked_sub(1) {
                                Some(streams) => streams,
                                None => {
                                    log::warn!("Decrementing an open streams count failed due to an underflow.");
                                    return Err(H2Error::Connection(ErrorCode::InternalError));
                                }
                            };

                            if self.pending_gracefull_shutdown && self.open_streams==0 {
                                log::info!("H2 gracefull shutdown successfull. (Last stream closed) Closing connection.");
                                return Ok(())
                            }

                            if let Some(stream) = self.streams.get_mut(&id) {
                                stream.closed = true;
                            }
                        }
                        ConnectionReq::StreamClosed(id) => {
                            self.streams.remove(&id);
                        }
                    }

                    continue;
                }
            };

            /* Handle the frame */
            match frame.header.r#type {
                FrameType::Data => {
                    let frame: DataFrame = frame.try_into()?;

                    log::debug!("Received a data frame: {:?}", frame);

                    // If we receive a data frame on a stream that has been closed for some time,
                    // we treat this as a connection error.
                    let stream = self.streams.get(&frame.stream).ok_or_else(|| H2Error::Connection(ErrorCode::ProtocolError))?;

                    let amount: i32 = frame.data.len().try_into().map_err(|_| H2Error::Connection(ErrorCode::ProtocolError))?;

                    if amount>self.peer_window {
                        log::warn!("Remote peer exceeded the global window! amount: {}, global_window: {}", amount, self.peer_window);
                        return Err(H2Error::Connection(ErrorCode::FlowControlError));
                    }

                    stream.send_data(frame)?;

                    self.peer_window = self.peer_window.checked_sub(amount).ok_or_else(|| H2Error::Connection(ErrorCode::InternalError))?;
                }
                FrameType::Headers => {
                    let frame: HeadersFrame = frame.try_into()?;

                    log::debug!("Received a headers frame: {:?}", frame);

                    let stream = frame.stream;

                    // Client initiated streams must be odd-numbered.
                    if stream%2==0 {
                        log::warn!("DEBUG - H2 Peer initiated a stream with an even id: {}", stream);
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }

                    // Either the header is a regular header or a trailer.
                    // If it is a trailer it must belong to a not (fully!) closed stream. Otherwise it opens another stream.
                    // The stream handler will take care of ensuring a trailer is valid.
                    // If it is a new stream, it must not belong to one of the (implicitly) closed streams.
                    if self.streams.get(&stream).is_none() {
                        // Refuse the stream and close the connection if the streams is lower then the greatest open stream.
                        if self.biggest_open_stream>=frame.stream {
                            log::warn!(
                                "DEBUG - H2 Peer tried to open a stream that is lower then the biggest open stream. biggest: {}, stream: {}",
                                self.biggest_open_stream, frame.stream,
                            );
                            return Err(H2Error::Connection(ErrorCode::ProtocolError));
                        }
                    }

                    let stream_end = frame.end_stream;

                    let written = self.header_buffer.write(frame.data.as_ref()).unwrap(); // Can not fail.
                    if written!=frame.data.len() {
                        log::warn!("Header buffer is full!");
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }

                    if !frame.end_headers {
                        loop {
                            let frame = self.read_next_frame().await?;

                            if frame.header.r#type!=FrameType::Continuation {
                                return Err(H2Error::Connection(ErrorCode::ProtocolError));
                            }

                            let frame: ContinuationFrame = frame.try_into()?;

                            if frame.stream!=stream {
                                return Err(H2Error::Connection(ErrorCode::ProtocolError));
                            }

                            log::debug!("Received a continuation frame: {:?}", frame);

                            let written = self.header_buffer.write(frame.data.as_ref()).unwrap(); // Can not fail.
                            if written!=frame.data.len() {
                                log::warn!("Header buffer is full!");
                                return Err(H2Error::Connection(ErrorCode::ProtocolError));
                            }

                            if frame.end_headers {
                                break;
                            }
                        }
                    }

                    // Refuse the new stream if we are in shutdown mode.
                    if self.pending_gracefull_shutdown {
                        self.header_buffer.reset();

                        self.handle_possible_stream_error(Err(H2Error::Stream(ErrorCode::RefusedStream)), stream).await?;

                        continue;
                    }

                    // Refuse the stream if it violates the max concurrent streams setting.
                    if let Some(max_concurrent_streams) = self.config.h2_settings.max_concurrent_streams {
                        if self.open_streams>=max_concurrent_streams {
                            self.header_buffer.reset();

                            self.handle_possible_stream_error(Err(H2Error::Stream(ErrorCode::RefusedStream)), stream).await?;

                            continue;
                        }
                    }

                    let mut req = match self.read_request().await {
                        Ok(req) => req,
                        Err(err) => {
                            self.handle_possible_stream_error(Err(err), stream).await?;

                            continue;
                        }
                    };

                    log::info!("New request: {:?}", req);

                    /* Add additional extensions */
                    let extensions = req.extensions_mut();
                    extensions.insert(Globals::from(Arc::clone(&self.globals)));
                    extensions.insert(Locals::from(Arc::clone(&self.ext)));
                    extensions.insert(Pool::from(Arc::clone(&self.pool)));

                    let config = Arc::clone(&self.config);

                    let window = self.remote_settings.initial_window_size.try_into().map_err(|_| H2Error::Connection(ErrorCode::ProtocolError))?;
                    let peer_window = self.local_settings.initial_window_size.try_into().map_err(|_| H2Error::Connection(ErrorCode::ProtocolError))?;

                    if stream>self.biggest_open_stream {
                        self.biggest_open_stream = stream;
                    }

                    self.open_streams = match self.open_streams.checked_add(1) {
                        Some(streams) => streams,
                        None => {
                            log::warn!("Incrementing an open streams count failed due to an overflow.");
                            return Err(H2Error::Connection(ErrorCode::InternalError));
                        }
                    };

                    let stream = Stream::new(
                        stream,
                        StreamState::Open,
                        config,
                        window,
                        peer_window,
                        conn_tx.clone(),
                    );

                    let handler = Arc::clone(&handler);

                    let stream_handle = stream.run(
                        handler,
                        req,
                        stream_end,
                    );

                    self.streams.insert(frame.stream, stream_handle);
                }
                FrameType::Priority => {
                    let frame: PriorityFrame = frame.try_into()?;

                    log::debug!("Received a priority frame: {:?}", frame);
                    // TODO Ignored for now...
                }
                FrameType::RstStream => {
                    let frame: RstStreamFrame = frame.try_into()?;

                    log::debug!("Received a reset stream frame: {:?}", frame);
                }
                FrameType::Settings => {
                    let frame: SettingsFrame = frame.try_into()?;

                    log::debug!("Received a settings frame: {:?}", frame);

                    // Acknowledge the settings if necessary.
                    // Otherwise apply them.
                    if !frame.ack {
                        let dif = self.remote_settings.apply_settings_frame(frame)?;

                        for (_, stream) in self.streams.iter_mut() {
                            stream.update_window(dif)?;
                        }

                        let mut frame = SettingsFrame::default();
                        frame.ack = true;
                        self.write_frame(frame).await?;
                    } else {
                        log::debug!("Received SETTINGS ACK.");

                        self.settings_ack_timeout = None;
                    }
                }
                FrameType::PushPromise => {
                    log::debug!("Received a push promise frame!");
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }
                FrameType::Ping => {
                    let mut frame: PingFrame = frame.try_into()?;

                    log::debug!("Received a ping frame: {:?}", frame);

                    if !frame.ack {
                        frame.ack = true;
                        self.write_frame(frame).await?;
                    } else {
                        if frame.data!=self.config.h2_ping_payload {
                            return Err(H2Error::Connection(ErrorCode::ProtocolError));
                        }

                        self.ping_ack_timeout = None;
                    }
                }
                FrameType::GoAway => {
                    let frame: GoAwayFrame = frame.try_into()?;

                    log::info!("Received a go away frame: {:?}", frame);
                    break // TODO graceful handling?
                }
                FrameType::WindowUpdate => {
                    let frame: WindowUpdateFrame = frame.try_into()?;

                    log::debug!("Received a window update frame: {:?}", frame);

                    let amount: i32 = frame.window_size_increment.try_into().map_err(|_| H2Error::Connection(ErrorCode::ProtocolError))?;

                    if frame.stream==0 {
                        self.update_global_window(amount)?;
                    } else {
                        if let Some(stream) = self.streams.get_mut(&frame.stream) {
                            let result = stream.update_window(amount);
                            self.handle_possible_stream_error(result, frame.stream).await?;
                        }
                    }

                    self.distribute_window()?;
                }
                FrameType::Continuation => {
                    log::debug!("Received an unexpected Continuation frame!");
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }
            }
        }

        // Exit cleanly.
        Ok(())
    }


    /// A H2 error can either relate to a single stream or the entire connection.
    /// If the error does only relate to a single stream, we try to handle it here
    /// without closing the entire connection.
    async fn handle_possible_stream_error(
        &mut self,
        result: H2Result<()>,
        id: u32,
    ) -> H2Result<()> {
        if let Err(H2Error::Stream(error_code)) = result {
            log::warn!("DEBUG - There was a stream error on a stream. error_code: {:?}, id: {}", error_code, id);

            // A stream error on the psudo stream 0 is not possible.
            if id==0 {
                log::warn!("There was a stream error on stream 0. error_code: {:?}", error_code);
                return Err(H2Error::Connection(ErrorCode::InternalError));
            }

            if let Some(stream) = self.streams.get_mut(&id) {
                stream.closed = true;

                let req = StreamReq::Reset;

                if let Err(_) = stream.tx.send(req) {
                    self.streams.remove(&id);
                }
            }

            let error_code = Some(error_code);

            let frame = RstStreamFrame {
                stream: id,
                error_code,
            };

            self.write_frame(frame).await?;

            // TODO are we sure the stream gets definatly closed on our side?
            // Do we have to wait or can we close it immediately? (consult RFC)

            return Ok(());
        }

        result
    }

    /// Distributes the available global window to all streams waiting for some window
    /// becoming available. It gets distributed by the FIFO principle.
    /// Gets called whenever the available global window increases.
    fn distribute_window(&mut self) -> H2Result<()> {
        loop {
            // Abort if there is no more window to distribute.
            if 0>=self.global_window {
                break;
            }

            // Get the first request pair. For security purposes, remove it
            // if it demands an empty window and continue.
            let pair = match self.pending_window_req.get_mut(0) {
                Some(pair) => pair,
                None => return Ok(()), // No request, so we can abort here.
            };
            if 0>=pair.1 {
                self.pending_window_req.remove(0);
                continue;
            }

            let id = pair.0;

            // Retrieve the corresponding stream handle.
            let stream = match self.streams.get_mut(&id) {
                Some(stream) => stream,
                None => {
                    // The stream does no longer exists, so we can drop the window request.
                    self.pending_window_req.remove(0);
                    continue;
                }
            };

            let amount = pair.1;

            // Find out how much we can give, and how much remains.
            let (amount, remaining) = if self.global_window>=amount {
                (amount, 0)
            } else {
                (self.global_window, amount-self.global_window)
            };

            // TODO graceful handling, maybe just dropping the stream handle?;
            stream.give_from_global_window(amount)?;

            self.update_global_window(-amount)?;

            if remaining>0 {
                let pair = self.pending_window_req.get_mut(0).ok_or_else(|| H2Error::Connection(ErrorCode::InternalError))?;

                pair.1 = remaining;
            } else {
                self.pending_window_req.remove(0);
            }
        }

        Ok(())
    }

    /// Updates the available global window by the given amount.
    fn update_global_window(&mut self, amount: i32) -> H2Result<()> {
        self.global_window = match self.global_window.checked_add(amount) {
            Some(window) => window,
            None => {
                log::error!("Over-/Underflow while changing the global window due to a get window request. now: {}, dif: {}", self.global_window, -amount);
                return Err(H2Error::Connection(ErrorCode::InternalError));
            }
        };

        Ok(())
    }

    /// Reads the H2 PREFACE from the connection. Gets send once after the creation
    /// of a connection.
    async fn read_preface(&mut self) -> std::io::Result<()> {
        let preface_len = HTTP2_CLIENT_PREFACE.len();

        while preface_len>self.buffer.available_for_read() {
            let n = self.buffer.read_from_reader(&mut self.reader).await?;
            if n==0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "Incomplete H2 preface was send."));
            }
        }

        if HTTP2_CLIENT_PREFACE!=(&self.buffer.as_ref()[..preface_len]) {
            return Err(Error::new(ErrorKind::InvalidData, "Invalid H2 preface was send."));
        }

        self.buffer.progress(preface_len);

        Ok(())
    }

    /// Reads the next frame from the connection, waiting until the entire
    /// frame got send over.
    /// Returns the raw frame on success or a connection level error.
    async fn read_next_frame(&mut self) -> H2Result<Frame> {
        log::debug!("read_next_frame triggered.");

        // First read and parse the frame header.
        while 9>self.buffer.available_for_read() {
            let n = self.buffer.read_from_reader(&mut self.reader).await
                .map_err(|_| H2Error::Connection(ErrorCode::ProtocolError))?;
            if n==0 {
                log::warn!("Incomplet H2 frame was send.");
                return Err(H2Error::Connection(ErrorCode::ProtocolError));
            }
        }

        let header = FrameHeaderRaw::from_slice(self.buffer.as_ref())?;
        self.buffer.progress(9);
        self.buffer.rotate_buffer();

        // Then read the frame payload.
        let payload_size = header.length as usize;
        loop {
            if self.buffer.available_for_read()>=payload_size {
                break; // good, we can proceed.
            }
            if self.buffer.available_for_write()==0 {
                // Bad, we have run out of memeory.
                log::warn!("Frame buffer is full.");
                return Err(H2Error::Connection(ErrorCode::ProtocolError));
            }

            let n = self.buffer.read_from_reader(&mut self.reader).await
                .map_err(|_| H2Error::Connection(ErrorCode::ProtocolError))?;
            if n==0 {
                log::warn!("Incomplet H2 frame was send.");
                return Err(H2Error::Connection(ErrorCode::ProtocolError));
            }
        }

        let payload = &self.buffer.as_ref()[..payload_size];

        let frame = Frame::from_header_and_payload(header, payload)?;

        self.buffer.progress(payload_size);
        self.buffer.rotate_buffer();

        Ok(frame)
    }

    async fn write_frame<F: Into<Frame>>(&mut self, frame: F) -> std::io::Result<()> {
        let frame: Frame = frame.into();

        // Write the header.
        let length: u64 = frame.payload.len() as u64;
        let length = &length.to_be_bytes()[5..8];
        self.writer.write_all(length).await?;
        self.writer.write_all(&[frame.header.r#type as u8]).await?;
        self.writer.write_all(&[frame.header.flags.bits()]).await?;
        self.writer.write_all(&frame.header.stream.to_be_bytes()).await?;

        // Write the payload.
        self.writer.write_all(&frame.payload[..]).await?;

        // Make sure that time critical frames arrive as fast as possible.
        // self.writer.flush().await?;

        Ok(())
    }

    /// After a full set of HEADER frames got send, parse the request from
    /// the internal header buffer with the help of the HPACK decoder.
    async fn read_request(&mut self) -> H2Result<http::Request<()>> {
        let mut header_list = match self.header_decoder.decode(self.header_buffer.as_ref()) {
            Ok(list) => list,
            Err(err) => {
                log::warn!("Could not decode header block. err: {:?}", err);
                return Err(H2Error::Connection(ErrorCode::CompresssionError));
            }
        };

        self.header_buffer.reset();

        // Configure a default req (GET, no body, no headers)
        let mut req = http::Request::new(());
        *req.version_mut() = http::Version::HTTP_2;

        // We must take not if we got all relevant pseudo headers.
        let mut got_method = false;
        let mut got_scheme = false;
        let mut got_path = false;
        let mut got_authority = false;

        let mut header_list = header_list.drain(..);

        // First, read out all pseudo headers.
        for (key, value) in &mut header_list {
            if !key.starts_with(b":") {
                if !got_method {
                    return Err(H2Error::Stream(ErrorCode::ProtocolError));
                }
                if req.method()==http::Method::CONNECT {
                    if got_scheme || got_path || !got_authority {
                        return Err(H2Error::Stream(ErrorCode::ProtocolError));
                    }
                } else {
                    if !got_scheme || !got_path {
                        return Err(H2Error::Stream(ErrorCode::ProtocolError));
                    }
                }

                Self::add_header_line(&mut req, key, value)?;
                break;
            }

            match &key[..] {
                b":method" => {
                    if got_method {
                        return Err(H2Error::Stream(ErrorCode::ProtocolError));
                    }

                    let method = http::Method::from_bytes(&value[..]).map_err(|_| H2Error::Stream(ErrorCode::ProtocolError))?;
                    *req.method_mut() = method;
                    got_method = true;
                },
                b":scheme" => {
                    if got_scheme {
                        return Err(H2Error::Stream(ErrorCode::ProtocolError));
                    }

                    got_scheme = true;
                },
                b":path" => {
                    if got_path {
                        return Err(H2Error::Stream(ErrorCode::ProtocolError));
                    }

                    let uri = http::Uri::try_from(&value[..]).map_err(|_| H2Error::Stream(ErrorCode::ProtocolError))?;
                    *req.uri_mut() = uri;
                    got_path = true;
                },
                b":authority" => {
                    if got_authority {
                        return Err(H2Error::Stream(ErrorCode::ProtocolError));
                    }

                    got_authority = true;
                },
                _ => return Err(H2Error::Stream(ErrorCode::ProtocolError)), // unknown pseudo header
            }
        }

        // Read all the regular headers.
        for (key, value) in &mut header_list {
            if key.starts_with(b":") {
                return Err(H2Error::Stream(ErrorCode::ProtocolError));
            }

            Self::add_header_line(&mut req, key, value)?;
        }

        Ok(req)
    }

    /// Utility function which adds binary header data to a http request object.
    fn add_header_line(
        req: &mut http::Request<()>,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> H2Result<()> {
        let name = HeaderName::from_bytes(&key[..]).map_err(|_| H2Error::Stream(ErrorCode::ProtocolError))?;
        let value = HeaderValue::from_bytes(&value[..]).map_err(|_| H2Error::Stream(ErrorCode::ProtocolError))?;

        req.headers_mut().append(name, value);

        Ok(())
    }

    async fn write_response(
        &mut self,
        resp: http::Response<()>,
        stream: u32,
        end_stream: bool,
    ) -> H2Result<()> {
        let status = resp.status().as_str().to_string();
        let priority = None;

        // First create all pseudo headers.
        let mut pseudo: Vec<(&[u8], &[u8])> = vec![(b":status", status.as_bytes())];

        // Then create the iterator over the response headers.
        let headers = resp.headers().iter().map(|(name, value)| (name.as_str().as_bytes(), value.as_bytes()));

        let full_headers = pseudo.drain(..).chain(headers);

        let mut data_store = Some(self.header_encoder.encode(full_headers));

        let mut first = true;
        while let Some(mut data) = data_store.take() {
            let remaining = if data.len()>16_384 {
                Some(data.split_off(16_384))
            } else {
                None
            };

            let end_headers = !remaining.is_some();

            if first {
                first = false;

                let frame = HeadersFrame {
                    stream,
                    end_stream,
                    end_headers,
                    priority,
                    data,
                };

                if let Err(err) = self.write_frame(frame).await {
                    log::error!("Writing H2 frame failed. err: {:?}", err);
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }
            } else {
                let frame = ContinuationFrame {
                    stream,
                    end_headers,
                    data,
                };

                if let Err(err) = self.write_frame(frame).await {
                    log::error!("Writing H2 frame failed. err: {:?}", err);
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }
            }

            data_store = remaining;
        }

        Ok(())
    }
}



struct BackgroundRead {
    buffer: Option<Buffer>,
    sender: Option<mpsc::UnboundedSender<i32>>,
    receiver: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl BackgroundRead {
    fn new(
        sender: mpsc::UnboundedSender<i32>,
        receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Self {
        let sender = Some(sender);
        let receiver = Some(receiver);

        Self {
            buffer: None,
            sender,
            receiver,
        }
    }

    fn update_window(&self, amount: usize) {
        log::debug!("BackgroundRead.update_window triggered. amount: {}", amount);

        let amount: i32 = match amount.try_into() {
            Ok(amount) => amount,
            Err(_) => {
                log::warn!("Could not convert the size of a read buffer (H2 data) into an i32! amount: {}", amount);
                return;
            }
        };

        if let Some(sender) = self.sender.as_ref() {
            log::debug!("BackgroundRead - Sending update window. amount: {}", amount);
            sender.send(amount).unwrap_or(());
        }
    }
}

impl AsyncRead for BackgroundRead {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let inner = Pin::into_inner(self);

        // First check if we have some unread bytes.
        if let Some(mut buffer) = inner.buffer.take() {
            let available = buffer.available_for_read();
            if available>0 {
                log::debug!("H2 BackgroundRead - buffer had some unread bytes ready. available: {}", available);
                let amount = available.min(buf.remaining());

                buf.put_slice(&buffer.as_ref()[..amount]);
                buffer.progress(amount);

                // Drop it if's now empty, otherwise add it again.
                if buffer.available_for_read()>0 {
                    inner.buffer = Some(buffer);
                } else {
                    inner.update_window(buffer.inner().len());
                    drop(buffer);
                }

                return Poll::Ready(Ok(()));
            }

            // We forgot to drop the buffer. drop it and proceed.
            inner.update_window(buffer.inner().len());
            drop(buffer); // Happens automatically, but doesn't hurt to do it manually.
        }

        // Either we are waiting for the next buffer, or we are done.
        let receiver = match inner.receiver.as_mut() {
            Some(receiver) => receiver,
            None => return Poll::Ready(Err(Error::new(ErrorKind::BrokenPipe, "This object is no longer usable."))),
        };

        match receiver.poll_recv(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Some(buffer)) => {
                let mut buffer: Buffer = buffer.into();

                log::debug!("BackgroundRead - Received a buffer. len: {}", buffer.inner().len());

                let available = buffer.available_for_read();
                if available>0 {
                    log::debug!("H2 BackgroundRead - buffer has some unread bytes ready. available: {}", available);
                    let amount = available.min(buf.remaining());

                    buf.put_slice(&buffer.as_ref()[..amount]);
                    buffer.progress(amount);

                    // Drop it if's now empty, otherwise add it.
                    if buffer.available_for_read()>0 {
                        inner.buffer = Some(buffer);
                    } else {
                        inner.update_window(buffer.inner().len());
                        drop(buffer);
                    }
                } else {
                    log::warn!("H2 BackgroundRead - Received an empty buffer. buffer: {:?}", buffer);
                }

                return Poll::Ready(Ok(()));
            }
            Poll::Ready(None) => return Poll::Ready(Ok(())),
        }
    }
}

/// Makes sure that all buffered Data frames got processed for window accounting purposes.
impl Drop for BackgroundRead {
    fn drop(&mut self) {
        let sender = match self.sender.take() {
            Some(sender) => sender,
            None => return,
        };
        let mut receiver = match self.receiver.take() {
            Some(receiver) => receiver,
            None => return,
        };

        if let Some(buffer) = self.buffer.take() {
            let amount = buffer.into_inner().len();
            if let Ok(amount) = TryInto::<i32>::try_into(amount) {
                sender.send(amount).unwrap_or(());
            }
        }

        tokio::spawn(async move {
            while let Some(buffer) = receiver.recv().await {
                let amount = buffer.len();
                let amount = match TryInto::<i32>::try_into(amount) {
                    Ok(amount) => amount,
                    Err(err) => {
                        log::warn!("H2 BackgroundRead - Could not convert amount into i32. err: {:?}, amount: {}", err, amount);
                        continue;
                    }
                };
                if let Err(_) = sender.send(amount) {
                    log::warn!("H2 BackgroundRead - Could not send a window update. amount: {}", amount);
                    return; // Will fail in the future, so we abourt here.
                }
            }
        });
    }
}
