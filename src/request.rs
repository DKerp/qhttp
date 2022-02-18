use tokio::io::{AsyncRead, ReadBuf};

use chrono::prelude::*;

use std::pin::Pin;
use std::marker::Unpin;
use std::task::{Context, Poll};
use std::sync::Arc;
use std::ops::Deref;

use std::io::{Error, ErrorKind};

use http::Extensions;
use http::header::*;

use generic_pool::SyncPool;

use crate::util::byterange::*;
use crate::parse::*;
use crate::buffer::*;
use crate::HttpError;



#[derive(Debug, Clone, Copy)]
pub(crate) enum BodyType {
    NoBody,
    Fixed(usize),
    Chunked,
}



#[derive(Debug, Default)]
pub(crate) struct Globals(Arc<Extensions>);

impl From<Arc<Extensions>> for Globals {
    fn from(globals: Arc<Extensions>) -> Self {
        Self(globals)
    }
}

impl Deref for Globals {
    type Target = Extensions;

    fn deref(&self) -> &Self::Target {
        &(*self.0)
    }
}

#[derive(Debug, Default)]
pub(crate) struct Locals(Arc<Extensions>);

impl From<Arc<Extensions>> for Locals {
    fn from(locals: Arc<Extensions>) -> Self {
        Self(locals)
    }
}

impl Deref for Locals {
    type Target = Extensions;

    fn deref(&self) -> &Self::Target {
        &(*self.0)
    }
}

pub(crate) struct Pool(Arc<SyncPool>);

impl From<Arc<SyncPool>> for Pool {
    fn from(pool: Arc<SyncPool>) -> Self {
        Self(pool)
    }
}

impl Deref for Pool {
    type Target = SyncPool;

    fn deref(&self) -> &Self::Target {
        &(*self.0)
    }
}



/// An extension trait for the [`http::Request`] object which adds QHTTP specific methods to it.
pub trait HttpRequest: crate::sealed::Sealed {
    /// Retrieve the [`Extensions`] container with global variables.
    fn get_globals(&self) -> Option<&Extensions>;

    /// Retrieve the [`Extensions`] container with connection local variables.
    /// Note that they persist over multiple requests over the same connection.
    fn get_locals(&self) -> Option<&Extensions>;

    /// Get access to the global pool which can be used to recycle objects with
    /// high allocation costs.
    fn get_pool(&self) -> Option<&SyncPool>;


    /// Retrieve the first header with the provided name as a [`str`].
    ///
    /// Will return [None] if the first header is not a valid [`str`].
    fn get_header<H: AsHeaderName>(&self, name: H) -> Option<&str>;

    /// Retrieve all header fields with the provided name, concatenated by a single white space.
    ///
    /// Will skip any header which is not a valid [`String`].
    fn get_header_full<H: AsHeaderName>(&self, name: H) -> Option<String>;

    /// Retrieve all header fields with the provided name, concatenated by a the given seperator.
    ///
    /// Will skip any header which is not a valid [`String`].
    fn get_header_full_with_seperator<H: AsHeaderName>(&self, name: H, sep: &'static str) -> Option<String>;

    /// Find out if the client send a `Expect: 100-continue` header.
    ///
    /// Note that the QHTTP [`Server`](crate::Server) will automatically send the
    /// `Continue` status line if necessary.
    fn expects_continue(&self) -> bool;

    /// Retrieve the content length of the request body, if any.
    ///
    /// Will return [None] if the first header is not a valid [`usize`] number.
    fn get_content_length(&self) -> Option<usize>;

    /// Retrieve the `If-Modified-Since` header as a [`DateTime`] object, if any.
    ///
    /// Will return [None] if the first header is not a valid datetime string.
    fn get_if_modified_since(&self) -> Option<DateTime<FixedOffset>>;

    /// Retrieve the `If-Unmodified-Since` header as a [`DateTime`] object, if any.
    ///
    /// Will return [None] if the first header is not a valid datetime string.
    fn get_if_unmodified_since(&self) -> Option<DateTime<FixedOffset>>;

    /// Retrieve the `Range` header as a parsed collection of [`ByteRange`] objects, if any.
    ///
    /// Will return [`None`] if the `Range` header does not exist and return an
    /// `Bad Request` [`HttpError`] with the close flag __disabled__ if the header was invalid.
    fn get_range(&self) -> Result<Option<Vec<ByteRange>>, HttpError>;
}

impl<T> HttpRequest for http::Request<T> {
    fn get_globals(&self) -> Option<&Extensions> {
        if let Some(container) = self.extensions().get::<Globals>() {
            return Some(&**container)
        }

        None
    }

    fn get_locals(&self) -> Option<&Extensions> {
        if let Some(container) = self.extensions().get::<Locals>() {
            return Some(&**container)
        }

        None
    }

    fn get_pool(&self) -> Option<&SyncPool> {
        if let Some(container) = self.extensions().get::<Pool>() {
            return Some(&**container)
        }

        None
    }


    fn get_header<H: AsHeaderName>(&self, name: H) -> Option<&str> {
        if let Some(value) = self.headers().get(name) {
            return value.to_str().ok()
        }

        None
    }

    fn get_header_full<H: AsHeaderName>(&self, name: H) -> Option<String> {
        self.get_header_full_with_seperator(name, " ")
    }

    fn get_header_full_with_seperator<H: AsHeaderName>(&self, name: H, sep: &'static str) -> Option<String> {
        let full: Vec<String> = self.headers().get_all(name).iter().filter_map(|value| {
            value.to_str().ok().map(|s| String::from(s))
        }).collect();

        if full.is_empty() {
            return None;
        }

        let full = full.join(sep);

        Some(full)
    }

    fn expects_continue(&self) -> bool {
        match self.get_header(EXPECT) {
            Some(value) => value=="100-continue",
            None => false,
        }
    }

    fn get_content_length(&self) -> Option<usize> {
        if let Some(value) = self.get_header(CONTENT_LENGTH) {
            return value.parse::<usize>().ok();
        }

        None
    }

    fn get_if_modified_since(&self) -> Option<DateTime<FixedOffset>> {
        if let Some(value) = self.get_header(IF_MODIFIED_SINCE) {
            return DateTime::parse_from_rfc2822(value).ok();
        }

        None
    }

    fn get_if_unmodified_since(&self) -> Option<DateTime<FixedOffset>> {
        if let Some(value) = self.get_header(IF_UNMODIFIED_SINCE) {
            return DateTime::parse_from_rfc2822(value).ok();
        }

        None
    }

    fn get_range(&self) -> Result<Option<Vec<ByteRange>>, HttpError> {
        if let Some(value) = self.get_header(RANGE) {
            return value.parse::<ByteRanges>()
                .map(|ranges| Some(ranges.into_inner()) )
                .map_err(|_| HttpError{status: http::StatusCode::BAD_REQUEST, close: false});
        }

        Ok(None)
    }
}

pub(crate) trait ParseHttpRequest {
    fn determine_body_type(&self) -> std::io::Result<BodyType>;

    fn determine_body_type_h2(&self) -> std::io::Result<BodyType>;

    fn validate(&self) -> std::io::Result<()>;
}

impl<T> ParseHttpRequest for http::Request<T> {
    fn determine_body_type(&self) -> std::io::Result<BodyType> {
        if self.method().is_safe() {
            log::debug!("Request.determine_body_type: Got a SAFE request, deducing no body.");
            return Ok(BodyType::NoBody);
        }

        if let Some(value) = self.headers().get("transfer-encoding") {
            let value = value.to_str().map_err(|_| Error::new(ErrorKind::Other, "Invalid header value."))?;

            if value=="chunked" {
                log::debug!("Request.determine_body_type: Found a chunked transfer encoding header value, deducing a chunked body.");
                return Ok(BodyType::Chunked);
            } else {
                return Err(Error::new(ErrorKind::InvalidData, "Unknown transfer encoding."));
            }
        }

        if let Some(value) = self.headers().get("content-length") {
            let value = value.to_str().map_err(|_| Error::new(ErrorKind::Other, "Invalid header value."))?;
            let len = value.parse::<usize>().map_err(|_| Error::new(ErrorKind::Other, "Invalid header value. (Not an integer)"))?;

            if len>0 {
                log::debug!("Request.determine_body_type: Got a POSITIVE content length, deducing a fixed body.");
                return Ok(BodyType::Fixed(len));
            }
            log::debug!("Request.determine_body_type: Got content length of ZERO, deducing no body.");
        }

        Ok(BodyType::NoBody)
    }

    fn determine_body_type_h2(&self) -> std::io::Result<BodyType> {
        if self.method().is_safe() {
            log::debug!("Request.determine_body_type: Got a SAFE request, deducing no body.");
            return Ok(BodyType::NoBody);
        }

        if let Some(value) = self.headers().get("transfer-encoding") {
            let value = value.to_str().map_err(|_| Error::new(ErrorKind::Other, "Invalid header value."))?;

            if value=="chunked" {
                return Err(Error::new(ErrorKind::InvalidData, "Unknown transfer encoding."));
            }
        }

        if let Some(value) = self.headers().get("content-length") {
            let value = value.to_str().map_err(|_| Error::new(ErrorKind::Other, "Invalid header value."))?;
            let len = value.parse::<usize>().map_err(|_| Error::new(ErrorKind::Other, "Invalid header value. (Not an integer)"))?;

            if len>0 {
                log::debug!("Request.determine_body_type: Got a POSITIVE content length, deducing a fixed body.");
                return Ok(BodyType::Fixed(len));
            }

            return Ok(BodyType::NoBody);
        }

        // NOTE BodyType::Chunked in the H2 scenario means that the body has no fixed length,
        // bot NOT that it is used the chunked transfer-encoding.
        Ok(BodyType::Chunked)
    }

    fn validate(&self) -> std::io::Result<()> {
        Ok(())
    }
}


pub struct LimitReader<R: AsyncRead + Unpin + Send + Sync> {
    reader: R,
    max: usize,
    current: usize,
}

impl<R: AsyncRead + Unpin + Send + Sync> LimitReader<R> {
    pub fn new(reader: R, max: usize) -> Self {
        Self {
            reader: reader,
            max: max,
            current: 0,
        }
    }
}

impl<R: AsyncRead + Unpin + Send + Sync> AsyncRead for LimitReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut limit_reader = Pin::into_inner(self);

        // log::debug!(
        //     "LimitReader.poll_read: current: {}, max: {}, pointer: {}, read_pointer: {}, remaining: {}",
        //     limit_reader.current,
        //     limit_reader.max,
        //     limit_reader.body_buf_pointer,
        //     limit_reader.body_buf_read_pointer,
        //     buf.remaining(),
        // );

        // Check if we have already exceeded the limit
        if limit_reader.current>limit_reader.max {
            return Poll::Ready(Err(Error::new(ErrorKind::Other, "Implementation error (LimitReader has read more then max bytes!)")));
        }

        let remaining = limit_reader.max-limit_reader.current;

        // Return EOF.
        if remaining==0 {
            return Poll::Ready(Ok(()));
        }

        // If filling the buf is uncapable of exceeding the limit we can proceed regularely.
        // Otherwise we must create a new ReadBuf with a limited size.

        if remaining>=buf.remaining() {
            let begin = buf.filled().len();

            let pin: Pin<&mut R> = Pin::new(&mut limit_reader.reader);
            let poll = pin.poll_read(cx, buf);

            let end = buf.filled().len();

            if end>begin {
                let delta = end-begin;
                limit_reader.current = limit_reader.current+delta;
            }

            return poll
        } else {
            log::debug!("TEST - LimitReader.poll_read: created small_buf because buf was too big! remaining: {}, buf.remaining: {}", remaining, buf.remaining());
            let mut small_buf = buf.take(remaining);
            // let mut small_buf = ReadBuf::new(buf.initialize_unfilled_to(remaining));

            let pin: Pin<&mut R> = Pin::new(&mut limit_reader.reader);
            let poll = pin.poll_read(cx, &mut small_buf);

            let delta = small_buf.filled().len();
            drop(small_buf);
            unsafe {
                // SAFE, because we have just filled it with bytes.
                buf.assume_init(delta);
            }
            buf.advance(delta);
            limit_reader.current = limit_reader.current+delta;

            return poll
        }
    }
}


#[derive(Debug)]
enum ChunkedReaderStatus {
    WaitingForChunk,
    ReadingChunk,
    Done,
}

struct ChunkedReaderBuffer(Buffer);

pub(crate) struct ChunkedReader<R: AsyncRead + Unpin + Send + Sync> {
    reader: R,
    buffer: Option<Buffer>,

    status: ChunkedReaderStatus,

    current_chunk_size: usize,
    current_chunk_count: usize,

    pool: Arc<SyncPool>,
}

impl<R: AsyncRead + Unpin + Send + Sync> Drop for ChunkedReader<R> {
    fn drop(&mut self) {
        let buffer = self.buffer.take().unwrap();

        let buffer = ChunkedReaderBuffer(buffer);

        self.pool.put(buffer);

        log::debug!("ChunkedReader.drop: Sucessfully added buffer to the pool!");
    }
}

impl<R: AsyncRead + Unpin + Send + Sync> ChunkedReader<R> {
    pub fn new(reader: R, buffer_size: usize, pool: Arc<SyncPool>) -> Self {
        let buffer = match pool.get::<ChunkedReaderBuffer>() {
            Some(buffer) => {
                log::debug!("ChunkedReader.new: Successfully retrieved buffer from the pool!");
                let mut buffer = buffer.0;
                buffer.set_capacity(buffer_size);

                buffer
            }
            None => Buffer::with_capacity(buffer_size),
        };

        Self::new_with_buffer(reader, buffer, pool)
    }

    pub fn new_with_buffer(reader: R, buffer: Buffer, pool: Arc<SyncPool>) -> Self {
        let buffer = Some(buffer);

        Self {
            reader,
            buffer,
            status: ChunkedReaderStatus::WaitingForChunk,
            current_chunk_size: 0,
            current_chunk_count: 0,
            pool,
        }
    }

    /// A helper method to be used by the AsyncRead implementation.
    ///
    /// Tries to read some bytes from the internal reader into the internal buffer.
    /// Adjustes the buffer_pointer if the read was successfull, or returns a poll otherwise.
    fn read_to_buffer(&mut self, cx: &mut Context<'_>) -> Option<Poll<std::io::Result<()>>> {
        log::debug!("ChunkedReader.read_to_buffer triggered");

        let buffer = match self.buffer.as_mut() {
            Some(buffer) => buffer.as_mut(),
            None => return Some(Poll::Ready(Err(Error::new(ErrorKind::Other, "Empty buffer.")))),
        };

        // Check that the buffer is not yet full. Otherwise abort.
        if buffer.len()==0 {
            return Some(Poll::Ready(Err(Error::new(ErrorKind::Other, "The internal buffer was filled. The chunks are too big!"))));
        }

        let buffer = &mut buffer[..1];

        // Try to read data to the buffer.
        // Return a poll result if no data was read, indicating to the calling function
        // that it should return early.
        let mut readbuf = ReadBuf::new(buffer);
        let pin: Pin<&mut R> = Pin::new(&mut self.reader);
        match pin.poll_read(cx, &mut readbuf) {
            Poll::Ready(Ok(())) => {
                let n = readbuf.filled().len();
                log::debug!("ChunkedReader.read_to_buffer successfull. We have read {} bytes.", n);
                if n>0 {
                    self.buffer.as_mut().unwrap().progress_write(n);

                    return None;
                } else {
                    return Some(Poll::Ready(Err(Error::new(ErrorKind::UnexpectedEof, "Incomplete chunk header."))));
                }
            }
            Poll::Ready(Err(err)) => return Some(Poll::Ready(Err(err))),
            Poll::Pending => return Some(Poll::Pending),
        }
    }

    fn read_data(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        log::debug!("ChunkedReader.read_data triggered.");

        let mut remaining = match self.chunk_remaining() {
            Ok(n) => n,
            Err(err) => return Poll::Ready(Err(err)),
        };

        // Make sure we do not accidentaly return an empty read.
        if remaining==0 {
            let err = Err(Error::new(ErrorKind::Other, "Implementation error."));
            return Poll::Ready(err);
        }

        if remaining>buf.remaining() {
            remaining = buf.remaining();
        }

        log::debug!("#1 - buf.filled.len(): {}", buf.filled().len());
        let mut readbuf = buf.take(remaining);
        log::debug!("#1 - readbuf.filled().len(): {}", readbuf.filled().len());
        let pin: Pin<&mut R> = Pin::new(&mut self.reader);
        match pin.poll_read(cx, &mut readbuf) {
            Poll::Ready(Ok(())) => {
                let n = readbuf.filled().len();
                if n==0 {
                    return Poll::Ready(Err(Error::new(ErrorKind::UnexpectedEof, "Incomplete chunk.")));
                }

                unsafe {
                    // SAFE, because we have just filled it with bytes.
                    buf.assume_init(n);
                }
                buf.advance(n);

                log::debug!("#2 - readbuf.filled().len(): {}, buf.filled.len(): {}", n, buf.filled().len());

                self.current_chunk_count = self.current_chunk_count+n;

                return Poll::Ready(Ok(()));
            }
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Pending => return Poll::Pending,
        }
    }

    fn read_chunk_header(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        log::debug!("ChunkedReader.read_chunk_header triggered.");

        if let ChunkedReaderStatus::WaitingForChunk = self.status {
            // Find the index where the header ends.
            // NOTE read_to_buffer guarantees that the buffer will have 1 more byte available.
            // Since the buffer will eventually fill, this loop will also eventually end.
            let idx = loop {
                let buffer = match self.buffer.as_ref() {
                    Some(buffer) => buffer.as_ref(),
                    None => return Poll::Ready(Err(Error::new(ErrorKind::Other, "Empty buffer."))),
                };

                match find_chunk_header_end(buffer)? {
                    Some(idx) => break idx,
                    None => {
                        // The end is not yet available. We are not done with reading the header.
                        if let Some(poll) = self.read_to_buffer(cx) {
                            return poll;
                        }
                    }
                }
            };

            let buffer = match self.buffer.as_ref() {
                Some(buffer) => buffer.as_ref(),
                None => return Poll::Ready(Err(Error::new(ErrorKind::Other, "Empty buffer."))),
            };

            // Parse the size of the current chunk.
            let size = parse_chunk_header(&buffer[..idx])?;

            // Adjust the buffer read pointer.
            self.buffer.as_mut().unwrap().progress(idx); // Safe, since it would have abourted already above otherwise.

            // Adjust the chunk count.
            self.current_chunk_size = size;
            self.current_chunk_count = 0;

            // Change the status.
            if size>0 {
                self.status = ChunkedReaderStatus::ReadingChunk;
                return self.read_data(cx, buf);
            } else {
                self.status = ChunkedReaderStatus::Done;
                return self.read_terminating_crlf(cx);
            }
        }

        log::error!("ChunkedReader.read_chunk_header: Got called with the wrong status: {:?}", self.status);
        Poll::Ready(Err(Error::new(ErrorKind::Other, "Implementation error.")))
    }

    fn chunk_remaining(&self) -> std::io::Result<usize> {
        let chunk_left = if self.current_chunk_count>self.current_chunk_size {
            log::error!(
                "ChunkedReader.read_chunk_data: Invalid chunk counters: count: {}, size: {}",
                self.current_chunk_count,
                self.current_chunk_size,
            );
            return Err(Error::new(ErrorKind::Other, "Implementation error."));
        } else {
            self.current_chunk_size-self.current_chunk_count
        };

        Ok(chunk_left)
    }

    fn read_chunk_data(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        log::debug!("ChunkedReader.read_chunk_data triggered.");

        if let ChunkedReaderStatus::ReadingChunk = self.status {
            let chunk_left = match self.chunk_remaining() {
                Ok(n) => n,
                Err(err) => return Poll::Ready(Err(err)),
            };

            // If the chunk has already been read, look for the CRNL after it.
            if chunk_left==0 {
                loop {
                    let buffer = match self.buffer.as_ref() {
                        Some(buffer) => buffer.as_ref(),
                        None => return Poll::Ready(Err(Error::new(ErrorKind::Other, "Empty buffer."))),
                    };

                    if 2>buffer.len() {
                        if let Some(poll) = self.read_to_buffer(cx) {
                            return poll;
                        }
                    } else {
                        break;
                    }
                }

                let buffer = match self.buffer.as_ref() {
                    Some(buffer) => buffer.as_ref(),
                    None => return Poll::Ready(Err(Error::new(ErrorKind::Other, "Empty buffer."))),
                };

                if buffer[0]==CR && buffer[1]==NL {
                    self.buffer.as_mut().unwrap().progress(2);
                    self.status = ChunkedReaderStatus::WaitingForChunk;
                    return self.read_chunk_header(cx, buf);
                } else {
                    return Poll::Ready(Err(Error::new(ErrorKind::InvalidData, "Chunk data did not end with a CRNL.")));
                }
            }

            return self.read_data(cx, buf)
        }

        log::error!("ChunkedReader.read_chunk_data: Got called with the wrong status: {:?}", self.status);
        Poll::Ready(Err(Error::new(ErrorKind::Other, "Implementation error.")))
    }

    fn read_terminating_crlf(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        log::debug!("ChunkedReader.read_chunk_terminating_crlf triggered.");

        if let ChunkedReaderStatus::Done = self.status {
            loop {
                let buffer = match self.buffer.as_ref() {
                    Some(buffer) => buffer.as_ref(),
                    None => return Poll::Ready(Err(Error::new(ErrorKind::Other, "Empty buffer."))),
                };

                if 2>buffer.len() {
                    if let Some(poll) = self.read_to_buffer(cx) {
                        return poll;
                    }
                } else {
                    break;
                }
            }

            let buffer = match self.buffer.as_ref() {
                Some(buffer) => buffer.as_ref(),
                None => return Poll::Ready(Err(Error::new(ErrorKind::Other, "Empty buffer."))),
            };

            if buffer[0]==CR && buffer[1]==NL {
                return Poll::Ready(Ok(()));
            } else {
                return Poll::Ready(Err(Error::new(ErrorKind::InvalidData, "Chunked body did not end with a CRNL.")));
            }
        }

        log::error!("ChunkedReader.read_terminating_crlf: Got called with the wrong status: {:?}", self.status);
        Poll::Ready(Err(Error::new(ErrorKind::Other, "Implementation error.")))
    }
}

impl<R: AsyncRead + Unpin + Send + Sync> AsyncRead for ChunkedReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let reader = Pin::into_inner(self);

        let poll = match reader.status {
            ChunkedReaderStatus::WaitingForChunk => reader.read_chunk_header(cx, buf),
            ChunkedReaderStatus::ReadingChunk => reader.read_chunk_data(cx, buf),
            ChunkedReaderStatus::Done => reader.read_terminating_crlf(cx),
        };

        log::debug!("Chunkedreader.poll_read - poll: {:?}", poll);

        poll
    }
}
