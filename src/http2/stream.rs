use super::*;

use crate::request::{LimitReader, BodyType, ParseHttpRequest};

use http::header::*;
use http::{StatusCode, HeaderValue};

use chrono::prelude::*;



#[derive(Debug, Clone, Copy)]
pub enum StreamState {
    Idle,
    ReservedLocal,
    ReservedRemote,
    Open,
    HalfClosedLocal,
    HalfClosedRemote,
    Closed,
}

impl StreamState {
    pub fn is_frame_forbidden(&self, frame_type: FrameType) -> H2Result<()> {
        match self {
            Self::Idle => {
                match frame_type {
                    FrameType::Headers | FrameType::Priority => return Ok(()),
                    _ => return Err(H2Error::Connection(ErrorCode::ProtocolError)),
                }
            }
            Self::ReservedLocal => {
                match frame_type {
                    FrameType::RstStream | FrameType::Priority | FrameType::WindowUpdate => return Ok(()),
                    _ => return Err(H2Error::Connection(ErrorCode::ProtocolError)),
                }
            }
            Self::ReservedRemote => {
                match frame_type {
                    FrameType::Headers |FrameType::RstStream | FrameType::Priority => return Ok(()),
                    _ => return Err(H2Error::Connection(ErrorCode::ProtocolError)),
                }
            }
            Self::Open => return Ok(()),
            Self::HalfClosedLocal => return Ok(()),
            Self::HalfClosedRemote => {
                match frame_type {
                    FrameType::RstStream | FrameType::Priority | FrameType::WindowUpdate => return Ok(()),
                    _ => return Err(H2Error::Stream(ErrorCode::StreamClosed)),
                }
            }
            Self::Closed => {
                match frame_type {
                    FrameType::RstStream | FrameType::Priority | FrameType::WindowUpdate => return Ok(()),
                    _ => return Err(H2Error::Stream(ErrorCode::StreamClosed)),
                }
            }
        }
    }
}


pub(crate) enum StreamReq {
    /// The answer (eventual) answer to a GetWindow request.
    /// Informs the stream that it has successfully aquired to provided amount
    /// from the global window.
    AvailableWindow(i32),
    /// A stream level window update, either received by the peer or triggered by
    /// an update of the peers initial_window_size setting.
    /// Increases/Decreases the stream window by the given amount.
    WindowUpdate(i32),
    /// A data frame belonging to a request body.
    /// Gets passed on to the request handler trough the BackgroundRead facility.
    RequestBody(DataFrame),
    /// A stream level reset, received from the peer.
    Reset,
}

pub(crate) struct Stream {
    id: u32,
    state: StreamState,
    config: Arc<Config>,
    available_window: i32,
    window: i32,
    peer_window: i32,
    conn_tx: mpsc::UnboundedSender<ConnectionReq>,
    background_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    window_rx: Option<mpsc::UnboundedReceiver<i32>>,
    tx: mpsc::UnboundedSender<StreamReq>,
    rx: mpsc::UnboundedReceiver<StreamReq>,
}

impl Stream {
    pub(crate) fn new(
        id: u32,
        state: StreamState,
        config: Arc<Config>,
        window: i32,
        peer_window: i32,
        conn_tx: mpsc::UnboundedSender<ConnectionReq>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        Self {
            id,
            state,
            config,
            available_window: 0,
            window,
            peer_window,
            conn_tx,
            background_tx: None,
            window_rx: None,
            tx,
            rx,
        }
    }

    pub(crate) fn run(
        self,
        handler: Arc<Box<dyn Handler + 'static>>,
        req: http::Request<()>,
        stream_end: bool,
    ) -> StreamHandle {
        let handle = StreamHandle {
            tx: self.tx.clone(),
            closed: false,
        };

        tokio::spawn(async move {
            self.run_inner(handler, req, stream_end).await;
        });

        handle
    }

    async fn run_inner(
        mut self,
        handler: Arc<Box<dyn Handler + 'static>>,
        http_req: http::Request<()>,
        stream_end: bool,
    ) {
        // Perform the regular handleing of the stream, extracting any error which might accour.
        let error: Option<H2Error> = match self.main(handler, http_req, stream_end).await {
            Ok(()) => None,
            Err(err) => {
                log::warn!("Stream - main handle returned error. err: {:?}, id: {}", err, self.id);

                Some(err)
            }
        };

        // If for some reason we still have some global window aquired, give it back.
        if self.available_window>0 {
            let req = ConnectionReq::PutWindow(self.available_window);

            self.available_window = 0;

            if let Err(_) = self.conn_tx.send(req) {
                log::warn!("Sending PutWindow request to connections main task failed.");
                return;
            }
        }

        // Inform the connection handler that the stream is now closed.
        let req = ConnectionReq::StreamClosing(self.id, error);

        if let Err(_) = self.conn_tx.send(req) {
            log::warn!("Sending StreamClosing event to connections main task failed.");
            return;
        }

        // TODO handle some special frames and perform window accounting...

        // Inform the connection handler that the stream handler will now exit.
        let req = ConnectionReq::StreamClosed(self.id);

        if let Err(_) = self.conn_tx.send(req) {
            log::warn!("Sending StreamClosed event to connections main task failed.");
            return;
        }

        log::debug!("Stream handle ended. id: {}", self.id);
    }

    async fn main(
        &mut self,
        handler: Arc<Box<dyn Handler + 'static>>,
        http_req: http::Request<()>,
        stream_end: bool,
    ) -> H2Result<()> {
        let mut resp = self.get_response(handler, http_req, stream_end).await?;

        self.finish_response(&mut resp)?;

        // Extract the body.
        let (parts, body) = resp.into_parts();
        let resp = http::Response::from_parts(parts, ());

        let end_stream = body.body.is_none();

        // Send back the response.
        let resp = ConnectionReq::Response((resp, self.id, end_stream));

        if let Err(_) = self.conn_tx.send(resp) {
            log::debug!("Sending response to connections main task failed.");
            return Err(H2Error::Connection(ErrorCode::ProtocolError));
        }

        if end_stream {
            log::debug!("Stream - Response was send. There is no body, so we abort early. id: {}", self.id);
            return Ok(())
        }
        log::debug!("Stream - Response was send. Will now try to send the body. window: {}, id: {}", self.window, self.id);

        self.send_body(body).await?;

        Ok(())
    }

    async fn get_response(
        &mut self,
        handler: Arc<Box<dyn Handler + 'static>>,
        http_req: http::Request<()>,
        stream_end: bool,
    ) -> H2Result<http::Response<Body>> {
        /* Continue parsing and validating the request */
        let body_type = match http_req.determine_body_type_h2() {
            Ok(body_type) => body_type,
            Err(err) => {
                log::debug!("Error while determining body type. err: {:?}, req: {:?}", err, http_req);
                return Ok(Self::generate_resp(StatusCode::BAD_REQUEST));
            }
        };
        if let Err(err) = http_req.validate() {
            log::debug!("Determined an invalid request. err: {:?}, req: {:?}", err, http_req);
            return Ok(Self::generate_resp(StatusCode::BAD_REQUEST));
        }

        log::debug!("H2 body_type: {:?}, id: {}", body_type, self.id);

        /* Add the costum body */
        let body = if stream_end {
            Body::default()
        } else {
            match body_type {
                BodyType::NoBody => Body::default(),
                body_type => {
                    let size = if let BodyType::Fixed(size) = body_type {
                        Some(size)
                    } else {
                        None
                    };

                    let (background_tx, background_rx) = mpsc::unbounded_channel();
                    let (window_tx, window_rx) = mpsc::unbounded_channel();
                    self.background_tx = Some(background_tx);
                    self.window_rx = Some(window_rx);

                    let background_read = BackgroundRead::new(
                        window_tx,
                        background_rx,
                    );

                    let body: Box<dyn AsyncRead + Unpin + Send + Sync + 'static> = match size {
                        Some(size) => Box::new(LimitReader::new(background_read, size)),
                        None => Box::new(background_read),
                    };

                    Body {
                        body: Some(body),
                        body_len: size,
                        finish_on_drop: false,
                    }
                }
            }
        };
        let http_req = http_req.map(move |_| body);

        // Execute the handler.
        let duration = self.config.handler_timeout;
        let mut task = tokio::spawn(async move {
            timeout(duration, handler.handle(http_req)).await
        });

        // Wait for the response and run the necessary background tasks.
        let result = loop {
            tokio::select! {
                result = self.main_select() => {
                    result?;

                    continue;
                }
                result = &mut task => break result,
            }
        };

        /* Ensure the task did not panic */
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                log::error!("Request handler task paniced! err: {:?}", err);
                return Ok(Self::generate_resp(StatusCode::INTERNAL_SERVER_ERROR));
            }
        };

        /* Evaluate the result of the handle task, extracting the resp if any */
        let resp = match result {
            Ok(handle_result) => {
                match handle_result {
                    Ok(resp) => resp,
                    Err(err) => {
                        log::error!("Error while handling request: {:?}", err);

                        let mut resp = http::Response::new(Body::default());

                        *resp.status_mut() = err.status;
                        resp.headers_mut().insert(CONTENT_LENGTH, http::HeaderValue::from_static("0"));

                        resp
                    }
                }
            }
            Err(_) => {
                log::warn!("Request handler timed out!");
                return Ok(Self::generate_resp(StatusCode::INTERNAL_SERVER_ERROR));
            }
        };

        Ok(resp)
    }

    fn finish_response(
        &self,
        resp: &mut http::Response<Body>,
    ) -> H2Result<()> {
        /* Add the date header if necessary */
        if let None = resp.headers().get(DATE) {
            let date = Utc::now().to_rfc2822();
            let date = String::from(&date[0..(date.len()-5)])+"GMT";
            let date = HeaderValue::from_bytes(date.as_bytes())
                .map_err(|_| H2Error::Stream(ErrorCode::InternalError))?;

            resp.headers_mut().insert(DATE, date);
        }

        Ok(())
    }

    async fn send_body(&mut self, mut body: Body) -> H2Result<()> {
        let mut body_buffer = Buffer::with_capacity(16_384); // TODO

        // Send the body.
        loop {
            tokio::select! {
                result = self.main_select() => {
                    result?;
                }
                amount = body_buffer.read_from_reader(&mut body) => {
                    let amount = match amount {
                        Ok(amount) => amount,
                        Err(err) => {
                            log::error!("Error while reading H2 response body. err: {:?}", err);
                            return Err(H2Error::Stream(ErrorCode::InternalError));
                        }
                    };

                    log::debug!("Stream - read next body chunk. amount: {}, id: {}", amount, self.id);

                    if amount==0 {
                        let frame = DataFrame {
                            stream: self.id,
                            end_stream: true,
                            data: Vec::new(),
                        };

                        let resp = ConnectionReq::Frame(frame.into());

                        if let Err(_) = self.conn_tx.send(resp) {
                            log::warn!("Sending response body to connections main task failed.");
                            return Err(H2Error::Connection(ErrorCode::InternalError));
                        }

                        break;
                    }

                    let mut window_needed: i32 = amount.try_into().map_err(|_| H2Error::Stream(ErrorCode::InternalError))?;

                    // Wait until we could send the ENTIRE current chunk of data.
                    loop {
                        log::debug!("Stream - entered body loop iteration. id: {}", self.id);
                        let available = body_buffer.available_for_read();
                        if available==0 {
                            break;
                        }

                        // Check if we can request more window from the global window.
                        if window_needed>0 && self.window>0 {
                            let amount = if self.window>=window_needed {
                                window_needed
                            } else {
                                self.window
                            };

                            self.window = self.window - amount;
                            window_needed = window_needed - amount;

                            let resp = ConnectionReq::GetWindow((self.id, amount));

                            log::debug!("Stream - Will send a GetWindow request. resp: {:?}, id: {}", resp, self.id);

                            if let Err(_) = self.conn_tx.send(resp) {
                                log::warn!("Sending window request to connections main task failed.");
                                return Err(H2Error::Connection(ErrorCode::InternalError));
                            }
                        }

                        if self.available_window>0 {
                            log::debug!("Stream - we have window available. Will use it to send data. available: {}, window: {}, id: {}", available, self.available_window, self.id);
                            let window: usize = self.available_window.try_into().map_err(|_| H2Error::Stream(ErrorCode::InternalError))?;
                            let amount: usize = if window>available {
                                available
                            } else {
                                window
                            };

                            log::debug!("Stream - amount send: {}, id: {}", amount, self.id);

                            let frame = DataFrame {
                                stream: self.id,
                                end_stream: false,
                                data: body_buffer.as_ref()[..amount].to_vec(),
                            };

                            body_buffer.progress(amount);
                            let amount: i32 = amount.try_into().map_err(|_| H2Error::Stream(ErrorCode::InternalError))?;
                            self.available_window = self.available_window - amount;

                            let resp = ConnectionReq::Frame(frame.into());

                            if let Err(_) = self.conn_tx.send(resp) {
                                log::warn!("Sending response body to connections main task failed.");
                                return Err(H2Error::Connection(ErrorCode::InternalError));
                            }

                            continue;
                        }

                        log::debug!("Stream - Will now wait for the next window update... id: {}", self.id);

                        // Wait for the next event, hoping that it is a window update.
                        self.main_select().await?;
                    }

                    log::debug!("Stream - exited body loop iteration. id: {}", self.id);
                }
            }
        }

        Ok(())
    }

    async fn main_select(&mut self) -> H2Result<()> {
        if let Some(window_rx) = self.window_rx.as_mut() {
            tokio::select! {
                req = self.rx.recv() => {
                    let req = req.ok_or_else(|| H2Error::Connection(ErrorCode::InternalError))?;

                    self.handle_req(req)?;
                }
                amount = window_rx.recv() => {
                    let amount = match amount {
                        Some(amount) => amount,
                        None => {
                            self.window_rx = None;
                            return Ok(());
                        }
                    };

                    self.peer_window = self.peer_window.checked_add(amount).ok_or_else(|| H2Error::Connection(ErrorCode::InternalError))?;

                    let resp = ConnectionReq::UpdatePeerWindow((self.id, amount));

                    log::debug!("Stream - Will send a UpdatePeerWindow request. resp: {:?}, id: {}", resp, self.id);

                    if let Err(_) = self.conn_tx.send(resp) {
                        log::warn!("Sending window request to connections main task failed.");
                        return Err(H2Error::Connection(ErrorCode::InternalError));
                    }
                }
            }
        } else {
            let req = self.rx.recv().await;
            let req = req.ok_or_else(|| H2Error::Connection(ErrorCode::InternalError))?;

            self.handle_req(req)?;
        }

        Ok(())
    }

    fn handle_req(&mut self, req: StreamReq) -> H2Result<()> {
        match req {
            StreamReq::RequestBody(frame) => {
                log::debug!("Received a request body frame. len {:?}", frame.data.len());

                // Abort early if the request did not contain a body.
                let background_tx = self.background_tx.as_ref().ok_or_else(|| H2Error::Connection(ErrorCode::ProtocolError))?;

                // Make sure the frame length is within the RFCs limits.
                let amount: i32 = frame.data.len().try_into().map_err(|_| H2Error::Connection(ErrorCode::ProtocolError))?;

                // Check if there is a flow control violation.
                if amount>self.peer_window {
                    log::warn!("Remote peer exceeded a stream window! amount: {}, stream_window: {}", amount, self.peer_window);
                    return Err(H2Error::Connection(ErrorCode::FlowControlError));
                }

                // Retrieve the payload (Vec<u8>).
                let data = frame.data;

                // Try to send the payload to the BackgroundRead instance.
                // Normally the BackgroundRead instance would inform the stream handle once the current data
                // chunk has been full consumed, which will trigger a window update allowing the peer to send
                // more data.
                // If sending fails (e.g. because the request body got dropped) we must trigger the window
                // update directly. We can also skip updating the peers stream window in this case.
                if let Err(_) = background_tx.send(data) {
                    log::debug!("Stream - Could not send a data buffer to the BackgroundRead. id: {}", self.id);

                    // The body got already dropped. Increase the global window diretly.
                    let resp = ConnectionReq::UpdatePeerWindow((self.id, amount));

                    log::debug!("Stream - Will send a UpdatePeerWindow request. resp: {:?}, id: {}", resp, self.id);

                    if let Err(_) = self.conn_tx.send(resp) {
                        log::warn!("Sending window request to connections main task failed.");
                        return Err(H2Error::Connection(ErrorCode::InternalError));
                    }

                    // Return early since we do not need to update the stream window.
                    return Ok(());
                }

                // Update the peers stream window.
                self.peer_window = self.peer_window.checked_sub(amount).ok_or_else(|| H2Error::Connection(ErrorCode::ProtocolError))?;
            }
            StreamReq::AvailableWindow(amount) => {
                log::debug!("Stream - received available window. amount: {}, id: {}", amount, self.id);

                // Update the amount of global window aquired by this stream.
                self.available_window = match self.available_window.checked_add(amount) {
                    Some(total) => total,
                    None => {
                        log::warn!("Over-/Underflow accoured due to an available window update! current: {}, update: {}", self.available_window, amount);
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }
                };

                log::debug!("Stream AVAILABLE window updated. window: {}, id: {}", self.available_window, self.id);
            }
            StreamReq::WindowUpdate(amount) => {
                log::debug!("Stream - received window. amount: {}, id: {}", amount, self.id);

                self.window = match self.window.checked_add(amount) {
                    Some(total) => total,
                    None => {
                        log::warn!("Over-/Underflow accoured due to an window update! current: {}, update: {}", self.window, amount);
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }
                };

                log::debug!("Stream window updated. window: {}, id: {}", self.window, self.id);
            }
            StreamReq::Reset => {
                return Err(H2Error::Stream(ErrorCode::NoError));
            }
        }

        Ok(())
    }

    fn generate_resp(status: StatusCode) -> http::Response<Body> {
        let mut resp = http::Response::new(Body::default());
        *resp.status_mut() = status;

        resp
    }
}

pub(crate) struct StreamHandle {
    pub tx: mpsc::UnboundedSender<StreamReq>,
    pub closed: bool,
}

impl StreamHandle {
    pub fn update_window(&self, amount: i32) -> H2Result<()> {
        // Inform the stream task of the now available window.
        let req = StreamReq::WindowUpdate(amount);

        if let Err(_) = self.tx.send(req) {
            log::warn!("Sending window update to stream task failed.");
            return Err(H2Error::Stream(ErrorCode::StreamClosed));
        }

        Ok(())
    }

    pub fn give_from_global_window(&self, amount: i32) -> H2Result<()> {
        // Inform the stream task of the now available window.
        let req = StreamReq::AvailableWindow(amount);

        if let Err(_) = self.tx.send(req) {
            log::warn!("Sending available window to stream task failed.");
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        Ok(())
    }

    pub fn send_data(&self, data: DataFrame) -> H2Result<()> {
        let req = StreamReq::RequestBody(data);

        if let Err(_) = self.tx.send(req) {
            log::warn!("Sending available window to stream task failed.");
            return Err(H2Error::Stream(ErrorCode::StreamClosed));
        }

        Ok(())
    }
}
