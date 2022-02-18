use crate::*;

use tokio::io::{AsyncWrite};

use std::io::{Error, ErrorKind};

use std::pin::Pin;
use std::marker::Unpin;
use std::task::{Context, Poll};



pub struct ChunkedWriter<'a> {
    writer: &'a mut BufWriter<Box<dyn AsyncWrite + Unpin + Send + Sync + 'static>>,
    buffer: Vec<u8>,
    buffer_pointer: usize,
    buffer_read_pointer: usize,
    header_buffer: String,
    header_buffer_read_pointer: usize,
    finishing: bool,
    finished: bool,
}

impl<'a> ChunkedWriter<'a> {
    pub(crate) fn new(
        writer: &'a mut BufWriter<Box<dyn AsyncWrite + Unpin + Send + Sync + 'static>>,
        buffer_size: usize,
    ) -> Self {
        let buffer = vec![0u8; buffer_size];

        Self {
            writer,
            buffer,
            buffer_pointer: 0,
            buffer_read_pointer: 0,
            header_buffer: String::with_capacity(64),
            header_buffer_read_pointer: 0,
            finishing: false,
            finished: false,
        }
    }

    fn write_inner(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.finished || self.finishing {
            return Poll::Ready(Err(Error::new(ErrorKind::BrokenPipe, "The ChunkedWriter has already been shutdown.")));
        }

        let len = self.buffer.len()-2;

        // If the buffer is full, flush it first before wrting to it.
        // We also do the same if the header is set, indicating an unfinished flush.
        if self.buffer_pointer>=len || !self.header_buffer.is_empty() {
            match self.flush_inner(cx) {
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) => (), // The buffer got flushed. Continue.
            }
        }

        let writable = len-self.buffer_pointer;

        if buf.len() > writable {
            self.buffer[self.buffer_pointer..len].copy_from_slice(&buf[..writable]);
            self.buffer_pointer = self.buffer_pointer+writable;

            return Poll::Ready(Ok(writable));
        } else {
            let end = self.buffer_pointer+buf.len();

            self.buffer[self.buffer_pointer..end].copy_from_slice(buf);
            self.buffer_pointer = end;

            return Poll::Ready(Ok(buf.len()));
        }
    }

    fn flush_inner(
        &mut self,
        cx: &mut Context<'_>
    ) -> Poll<std::io::Result<()>> {
        if self.finished {
            return Poll::Ready(Err(Error::new(ErrorKind::BrokenPipe, "The ChunkedWriter has already been shutdown.")));
        }

        // Create the chunk header if necessary.
        self.create_chunk();

        // At first we send the chunk header.
        if let Some(poll) = self.write_header(cx) {
            return poll;
        }

        // Then the chunk body.
        if let Some(poll) = self.write_body(cx) {
            return poll;
        }

        self.reset();

        AsyncWrite::poll_flush(Pin::new(&mut self.writer), cx)
    }

    fn write_header(
        &mut self,
        cx: &mut Context<'_>
    ) -> Option<Poll<std::io::Result<()>>> {
        loop {
            // Check if the chunk header got fully send.
            if self.header_buffer_read_pointer>=self.header_buffer.len() {
                break;
            }

            // Prepare the writer for calling AsyncWrite on it.
            let writer = Pin::new(&mut self.writer);
            let buf = &self.header_buffer.as_bytes()[self.header_buffer_read_pointer..];

            match AsyncWrite::poll_write(writer, cx, buf) {
                Poll::Ready(Ok(n)) => {
                    self.header_buffer_read_pointer = self.header_buffer_read_pointer+n;
                }
                Poll::Ready(Err(err)) => return Some(Poll::Ready(Err(err))),
                Poll::Pending => return Some(Poll::Pending),
            }
        }

        None
    }

    fn write_body(
        &mut self,
        cx: &mut Context<'_>
    ) -> Option<Poll<std::io::Result<()>>> {
        loop {
            // Check if the chunk body got fully send.
            if self.buffer_read_pointer>=self.buffer_pointer {
                break;
            }

            // Prepare the writer for calling AsyncWrite on it.
            let writer = Pin::new(&mut self.writer);
            let buf = &self.buffer[self.buffer_read_pointer..self.buffer_pointer];

            match AsyncWrite::poll_write(writer, cx, buf) {
                Poll::Ready(Ok(n)) => {
                    self.buffer_read_pointer = self.buffer_read_pointer+n;
                }
                Poll::Ready(Err(err)) => return Some(Poll::Ready(Err(err))),
                Poll::Pending => return Some(Poll::Pending),
            }
        }

        None
    }

    fn create_chunk(&mut self) {
        if self.header_buffer.is_empty() {
            self.header_buffer = format!("{:x}\r\n", self.buffer_pointer);
            self.buffer[self.buffer_pointer] = CR;
            self.buffer[self.buffer_pointer+1] = NL;
            self.buffer_pointer = self.buffer_pointer+2;
            // log::debug!("ChunkedWriter.create_chunk: self.header_buffer: {}", self.header_buffer);
        }
    }

    fn create_last_chunk(&mut self) {
        if self.header_buffer.is_empty() {
            self.header_buffer = String::from("0\r\n\r\n");
        }
    }

    fn reset(&mut self) {
        self.header_buffer.clear();
        self.header_buffer_read_pointer = 0;
        self.buffer_pointer = 0;
        self.buffer_read_pointer = 0;
    }

    fn shutdown_inner(
        &mut self,
        cx: &mut Context<'_>
    ) -> Poll<std::io::Result<()>> {
        if self.finished {
            return Poll::Ready(Err(Error::new(ErrorKind::BrokenPipe, "The ChunkedWriter has already been shutdown.")));
        }

        self.finishing = true;

        // Flush if necessary.
        if self.buffer_pointer>0 {
            match self.flush_inner(cx) {
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) => (), // The buffer got flushed. Continue.
            }
        }

        // Prepare the last chunk.
        self.create_last_chunk();

        // We send the final chunk header.
        if let Some(poll) = self.write_header(cx) {
            return poll;
        }

        match AsyncWrite::poll_flush(Pin::new(&mut self.writer), cx) {
            Poll::Ready(Ok(())) => (), // contiue
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Pending => return Poll::Pending,
        }

        self.finished = true;
        Poll::Ready(Ok(()))
    }
}

impl<'a> AsyncWrite for ChunkedWriter<'a> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8]
    ) -> Poll<std::io::Result<usize>> {
        Pin::into_inner(self).write_inner(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>
    ) -> Poll<std::io::Result<()>> {
        Pin::into_inner(self).flush_inner(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>
    ) -> Poll<std::io::Result<()>> {
        Pin::into_inner(self).shutdown_inner(cx)
    }
}
