use std::io::{Read, Write};
use std::ops::Index;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadBuf};



static ZERO: [u8;1<<20] = [0;1<<20];
const ZERO_LEN: usize = 1<<20;



pub(crate) trait BufferSlice: Index<usize, Output = u8> {
    fn len(&self) -> usize;

    fn slice(&self, begin: usize, end: usize) -> RingBufferSlice<'_>;

    fn to_vec(&self) -> Vec<u8>;
}



pub(crate) struct Buffer {
    inner: Option<Vec<u8>>,
    write_pointer: usize,
    read_pointer: usize,
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
         .field("inner.len", &self.inner().len())
         .field("write_pointer", &self.write_pointer)
         .field("read_pointer", &self.read_pointer)
         .finish()
    }
}

/// We wipe all data inside the buffer before it gets dropped for increased safety.
impl Drop for Buffer {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            let len = inner.len();

            if len>ZERO_LEN {
                for chunk in inner.chunks_mut(ZERO_LEN) {
                    let len = chunk.len();
                    chunk.copy_from_slice(&ZERO.as_ref()[..len]);
                }
            } else {
                inner.copy_from_slice(&ZERO.as_ref()[..len]);
            }
        }
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }
}

impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        &self.inner.as_ref().unwrap()[self.read_pointer..self.write_pointer]
    }
}

impl AsMut<[u8]> for Buffer {
    fn as_mut(&mut self) -> &mut [u8] {
        self.rotate_buffer();

        &mut self.inner.as_mut().unwrap()[self.write_pointer..]
    }
}

impl<T: Into<Vec<u8>>> From<T> for Buffer {
    fn from(buf: T) -> Self {
        let inner: Vec<u8> = buf.into();
        let write_pointer = inner.len();
        let inner = Some(inner);

        Self {
            inner,
            write_pointer,
            read_pointer: 0,
        }
    }
}

impl Buffer {
    const DEFAULT_CAPACITY: usize = 4096;

    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let inner = vec![0u8; capacity];
        let inner = Some(inner);

        Self {
            inner: inner,
            write_pointer: 0,
            read_pointer: 0,
        }
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        let inner = self.inner.as_mut().unwrap();

        if capacity!=inner.len() {
            *inner = vec![0u8; capacity];
        }

        self.reset()
    }

    pub fn into_inner(mut self) -> Vec<u8> {
        self.inner.take().unwrap()
    }

    pub fn reset(&mut self) {
        self.write_pointer = 0;
        self.read_pointer = 0;
    }

    pub async fn read_from_reader<R: AsyncReadExt + Send + Unpin>(&mut self, reader: &mut R) -> std::io::Result<usize> {
        self.rotate_buffer();

        log::debug!("Buffer.read_from_reader: buf len: {}", self.as_mut().len());
        let amount = reader.read(self.as_mut()).await?;

        self.write_pointer = self.write_pointer+amount;

        Ok(amount)
    }

    #[allow(dead_code)]
    pub async fn write_to_writer<W: AsyncWriteExt + Send + Unpin>(&mut self, writer: &mut W) -> std::io::Result<usize> {
        log::debug!("Buffer.write_to_writer: buf len: {}", self.as_ref().len());
        let amount = writer.write(self.as_ref()).await?;

        self.read_pointer = self.read_pointer+amount;

        Ok(amount)
    }

    pub fn inner(&self) -> &Vec<u8> {
        self.inner.as_ref().unwrap()
    }

    pub fn inner_mut(&mut self) -> &mut Vec<u8> {
        self.inner.as_mut().unwrap()
    }

    pub fn rotate_buffer(&mut self) {
        // log::warn!("Buffer.rotate_buffer - read_pointer: {}, write_pointer: {}", self.read_pointer, self.write_pointer);
        // If there no read bytes we can abort early.
        if self.read_pointer==0 {
            return
        }

        // If there are no unread bytes we can simply empty the buffer.
        if self.read_pointer>=self.write_pointer {
            self.read_pointer = 0;
            self.write_pointer = 0;
            return
        }

        // Perform the rotation and update the pointers accordingly.
        let begin = self.read_pointer;
        let end = self.write_pointer;
        self.inner_mut().copy_within(begin..end, 0);
        self.write_pointer = self.write_pointer-self.read_pointer;
        self.read_pointer = 0;
    }

    pub fn progress(&mut self, amount: usize) {
        // log::warn!("Buffer.progress - read_pointer: {}, amount: {}", self.read_pointer, amount);

        let new_read_pointer = self.read_pointer+amount;
        if new_read_pointer>self.write_pointer {
            self.read_pointer = self.write_pointer;
        } else {
            self.read_pointer = new_read_pointer;
        }

        // log::warn!("Buffer.progress - UPDATE FINISHED - read_pointer: {}", self.read_pointer);
    }

    pub fn progress_write(&mut self, amount: usize) {
        let inner = self.inner.as_ref().unwrap();

        // log::warn!("Buffer.progress_write - write_pointer: {}, amount: {}", self.write_pointer, amount);

        let new_write_pointer = self.write_pointer+amount;
        if new_write_pointer>inner.len() {
            self.write_pointer = inner.len();
        } else {
            self.write_pointer = new_write_pointer;
        }

        // log::warn!("Buffer.progress_write - UPDATE FINISHED - write_pointer: {}", self.write_pointer);
    }

    pub fn available_for_read(&self) -> usize {
        self.write_pointer-self.read_pointer
    }

    pub fn available_for_write(&self) -> usize {
        self.inner().len()-self.write_pointer+self.read_pointer
    }
}

impl Read for Buffer {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let available = self.available_for_read();
        if available==0 {
            return Ok(0);
        }

        let amount = if available>buf.len() {
            buf.len()
        } else {
            available
        };

        let begin = self.read_pointer;
        let end = begin+amount;
        (&mut buf[0..amount]).copy_from_slice(&self.inner()[begin..end]);

        self.read_pointer = self.read_pointer+amount;

        Ok(amount)
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.rotate_buffer();

        let available = self.available_for_write();
        if available==0 {
            return Ok(0);
        }

        let amount = if available>buf.len() {
            buf.len()
        } else {
            available
        };

        let begin = self.write_pointer;
        let end = begin+amount;
        (&mut self.inner_mut()[begin..end]).copy_from_slice(&buf[0..amount]);

        self.write_pointer = self.write_pointer+amount;

        Ok(amount)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}



#[allow(dead_code)]
pub(crate) struct RingBuffer {
    inner: Vec<u8>,
    write_pointer: usize,
    read_pointer: usize,
}

impl std::fmt::Debug for RingBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
         .field("len", &self.inner.len())
         .field("write_pointer", &self.write_pointer)
         .field("read_pointer", &self.read_pointer)
         .finish()
    }
}

impl<T: Into<Vec<u8>>> From<T> for RingBuffer {
    fn from(buf: T) -> Self {
        let inner: Vec<u8> = buf.into();

        Self {
            inner,
            write_pointer: 0,
            read_pointer: 0,
        }
    }
}

impl Index<usize> for RingBuffer {
    type Output = u8;

    fn index(&self, idx: usize) -> &Self::Output {
        self.get(idx).unwrap()
    }
}

impl BufferSlice for RingBuffer {
    fn len(&self) -> usize {
        self.available_for_read()
    }

    fn slice(&self, begin: usize, end: usize) -> RingBufferSlice<'_> {
        self.get_slice(begin, end)
    }

    fn to_vec(&self) -> Vec<u8> {
        if self.write_pointer>=self.read_pointer {
            return self.inner[self.read_pointer..self.write_pointer].to_vec();
        } else {
            let len1 = self.inner.len()-self.read_pointer;
            let len2 = self.write_pointer;

            let mut store = vec![0u8; len1 + len2];
            (&mut store[..len1]).copy_from_slice(&self.inner[self.read_pointer..]);
            (&mut store[len1..]).copy_from_slice(&self.inner[..self.write_pointer]);

            return store;
        }
    }
}

#[allow(dead_code)]
impl RingBuffer {
    pub fn with_len(len: usize) -> Self {
        let inner = vec![0u8; len];

        Self {
            inner: inner,
            write_pointer: 0,
            read_pointer: 0,
        }
    }

    pub fn resize(&mut self, len: usize) {
        self.inner.resize(len, 0u8);

        self.reset()
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.inner
    }

    pub fn inner(&self) -> &Vec<u8> {
        &self.inner
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter::new(self)
    }

    pub fn get_slice(&self, begin: usize, end: usize) -> RingBufferSlice {
        if begin>=self.inner.len() {
            panic!("The begin is beyond the slice boundary.");
        }
        if end>self.inner.len() {
            panic!("The end is beyond the slice boundary.");
        }
        RingBufferSlice::new(self, begin, end)
    }

    pub fn reset(&mut self) {
        self.write_pointer = 0;
        self.read_pointer = 0;
    }

    pub fn get(&self, idx: usize) -> Option<&u8> {
        let idx = if self.write_pointer>=self.read_pointer {
            if idx>=(self.write_pointer-self.read_pointer) {
                return None;
            }

            self.read_pointer+idx
        } else {
            let available1 = self.inner.len()-self.read_pointer;
            if available1>idx {
                self.read_pointer+idx
            } else if (available1+self.write_pointer)>idx {
                idx-available1
            } else {
                return None;
            }
        };

        self.inner.get(idx)
    }

    pub async fn read_from_reader<R: AsyncReadExt + Send + Unpin>(&mut self, reader: &mut R) -> std::io::Result<usize> {
        log::debug!("Buffer.read_from_reader: available for write: {}", self.available_for_write());
        let amount = reader.read(self.get_write_buf()).await?;

        self.progress_write(amount);

        Ok(amount)
    }

    pub fn copy_from_reader<R: Read>(&mut self, reader: &mut R) -> std::io::Result<usize> {
        let mut total_amount = 0usize;

        loop {
            let write_buf = self.get_write_buf();
            if write_buf.is_empty() {
                break;
            }

            let amount = Read::read(reader, write_buf)?;
            if amount==0 {
                break;
            }

            self.progress_write(amount);

            total_amount += amount;
        }

        Ok(total_amount)
    }

    fn write_to_buf(&mut self, buf: &mut [u8]) -> usize {
        let available = if self.write_pointer>=self.read_pointer {
            self.write_pointer-self.read_pointer
        } else {
            self.inner.len()-self.read_pointer
        };

        let amount = available.min(buf.len());

        let begin = self.read_pointer;
        let end = begin+amount;
        (&mut buf[0..amount]).copy_from_slice(&self.inner[begin..end]);

        self.progress_read(amount);

        return amount;
    }

    fn write_to_read_buf(&mut self, buf: &mut ReadBuf<'_>) -> usize {
        let amount = if self.write_pointer>=self.read_pointer {
            (self.write_pointer-self.read_pointer).min(buf.remaining())
        } else {
            (self.inner.len()-self.read_pointer).min(buf.remaining())
        };

        let begin = self.read_pointer;
        let end = begin + amount;

        buf.put_slice(&self.inner[begin..end]);

        self.progress_read(amount);

        amount
    }

    fn write_from_buf(&mut self, buf: &[u8]) -> usize {
        let write_buf = self.get_write_buf();
        let amount = write_buf.len().min(buf.len());

        (&mut write_buf[..amount]).copy_from_slice(&buf[..amount]);

        amount
    }

    fn get_write_buf(&mut self) -> &mut [u8] {
        if self.write_pointer>=self.read_pointer {
            &mut self.inner[self.write_pointer..]
        } else {
            &mut self.inner[self.write_pointer..self.read_pointer]
        }
    }

    pub fn progress_read(&mut self, amount: usize) {
        // log::warn!("Buffer.progress - read_pointer: {}, amount: {}", self.read_pointer, amount);

        // Add the amount.
        let new_pointer = self.read_pointer+amount;
        // Perform the wrapping.
        let new_pointer = new_pointer%self.inner.len();
        // Set the new value.
        self.read_pointer = new_pointer;

        // log::warn!("Buffer.progress - UPDATE FINISHED - read_pointer: {}", self.read_pointer);
    }

    fn progress_write(&mut self, amount: usize) {
        // log::warn!("Buffer.progress_write - write_pointer: {}, amount: {}", self.write_pointer, amount);

        // Add the amount.
        let new_pointer = self.write_pointer+amount;
        // Perform the wrapping.
        let new_pointer = new_pointer%self.inner.len();
        // Set the new value.
        self.write_pointer = new_pointer;

        // log::warn!("Buffer.progress_write - UPDATE FINISHED - write_pointer: {}", self.write_pointer);
    }

    pub fn available_for_read(&self) -> usize {
        if self.write_pointer>=self.read_pointer {
            self.write_pointer-self.read_pointer
        } else {
            self.inner.len()-self.read_pointer + self.write_pointer
        }
    }

    pub fn available_for_write(&self) -> usize {
        if self.write_pointer>=self.read_pointer {
            self.inner.len()-self.write_pointer + self.read_pointer
        } else {
            self.read_pointer-self.write_pointer
        }
    }
}

impl Read for RingBuffer {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut total_amount = 0;

        let amount = self.write_to_buf(buf);
        if amount>0 {
            total_amount += amount;

            if buf.len()>amount {
                let amount = self.write_to_buf(&mut buf[amount..]);
                total_amount += amount;
            }
        }

        Ok(total_amount)
    }
}

impl Write for RingBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut total_amount = 0;

        let amount = self.write_from_buf(buf);
        if amount>0 {
            total_amount += amount;

            if buf.len()>amount {
                let amount = self.write_from_buf(&buf[amount..]);
                total_amount += amount;
            }
        }

        Ok(total_amount)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}


#[allow(dead_code)]
pub struct RingBufferSlice<'a> {
    buffer: &'a RingBuffer,
    begin: usize,
    end: usize,
}

impl<'a> Index<usize> for RingBufferSlice<'a> {
    type Output = u8;

    fn index(&self, idx: usize) -> &Self::Output {
        let idx = idx+self.begin;

        if idx>=self.end {
            panic!("Invalid index: {}", idx);
        }

        &self.buffer[idx]
    }
}

impl<'a> BufferSlice for RingBufferSlice<'a> {
    fn len(&self) -> usize {
        self.end-self.begin
    }

    fn slice(&self, begin: usize, end: usize) -> RingBufferSlice<'_> {
        self.get_slice(begin, end)
    }

    fn to_vec(&self) -> Vec<u8> {
        let slice_len = self.end-self.begin;

        // We assume that begin and end are valid values, that is they are within
        // the boundaries of the sliced ring buffer.
        if self.buffer.write_pointer>=self.buffer.read_pointer {
            let begin = self.buffer.read_pointer+self.begin;
            let end = begin + slice_len;

            return self.buffer.inner[begin..end].to_vec();
        } else {
            let len1 = self.buffer.inner.len()-self.buffer.read_pointer;

            let mut store = vec![0u8; slice_len];

            // Does the slice begin in the first half of the ring buffer?
            if len1>self.begin {
                let begin = self.buffer.read_pointer+self.begin;
                let end = (begin + slice_len).min(self.buffer.inner.len());

                (&mut store[..len1-self.begin]).copy_from_slice(&self.buffer.inner[begin..end]);
            }

            // Does the slice end in the second half of the ring buffer?
            if self.end>len1 {
                let begin = (self.buffer.read_pointer+self.begin).checked_sub(self.buffer.inner.len()).unwrap_or(0);
                let end = self.end-len1;

                (&mut store[len1-self.begin..]).copy_from_slice(&self.buffer.inner[begin..end]);
            }

            return store;
        }
    }
}

#[allow(dead_code)]
impl<'a> RingBufferSlice<'a> {
    fn new(
        buffer: &'a RingBuffer,
        begin: usize,
        end: usize,
    ) -> Self {
        Self {
            buffer,
            begin,
            end,
        }
    }

    pub fn get_slice(&self, begin: usize, end: usize) -> RingBufferSlice {
        let begin = self.begin+begin;
        if end>self.end {
            panic!("RingBufferSlice - end value is beyond the slice boundary.");
        }
        RingBufferSlice::new(self.buffer, begin, end)
    }
}


#[allow(dead_code)]
pub struct Iter<'a> {
    buffer: &'a RingBuffer,
    counter: usize,
    available: usize,
}

#[allow(dead_code)]
impl<'a> Iter<'a> {
    fn new(buffer: &'a RingBuffer) -> Self {
        let available = buffer.available_for_read();

        Self {
            buffer,
            counter: 0,
            available,
        }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        if self.counter>=self.available {
            return None;
        }

        let item = self.buffer.get(self.counter);

        self.counter += 1;

        item.map(|&p| p)
    }
}
