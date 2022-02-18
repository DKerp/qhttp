use std::marker::Unpin;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::path::Path;
use std::io::Cursor;
use std::io::{Error, ErrorKind};

use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

#[cfg(feature = "json")]
use serde::{de::DeserializeOwned, Serialize};



/// The body that gets used to read/send http bodies. Implements `AsyncRead`.
#[derive(Default)]
pub struct Body {
    pub(crate) body: Option<Box<dyn AsyncRead + Unpin + Send + Sync + 'static>>,
    pub(crate) body_len: Option<usize>,
    pub(crate) finish_on_drop: bool,
}

impl Body {
    pub async fn new<R>(
        body: R,
        body_len: Option<usize>,
        finish_on_drop: bool,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + Sync + 'static,
    {
        Self {
            body: Some(Box::new(body)),
            body_len,
            finish_on_drop,
        }
    }

    pub async fn from_file(file: tokio::fs::File) -> std::io::Result<Self> {
        let body_len = file.metadata().await.map(|meta| Some(meta.len() as usize))?;

        Ok(Self {
            body: Some(Box::new(file)),
            body_len,
            finish_on_drop: false,
        })
    }

    pub async fn from_file_path(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = tokio::fs::File::open(path).await?;

        Self::from_file(file).await
    }

    #[cfg(feature = "json")]
    pub fn from_json<T: Serialize>(json: T) -> std::io::Result<Self> {
        let json = serde_json::to_vec(&json)
            .map_err(|err| {
                log::debug!("Serializing object failed. err: {:?}", err);

                Error::new(ErrorKind::Other, "Serializing response failed.")
            })?;
        let body_len = Some(json.len());

        let body = Cursor::new(json);

        Ok(Self {
            body: Some(Box::new(body)),
            body_len,
            finish_on_drop: false,
        })
    }

    pub fn activate_finish_on_drop(&mut self) {
        self.finish_on_drop = true;
    }

    pub async fn read_to_vec(&mut self, max: usize) -> std::io::Result<Vec<u8>> {
        // Check that the body is not too large and save space by using the lower value.
        let max = match self.body_len {
            Some(body_len) => {
                if body_len>max {
                    return Err(Error::new(ErrorKind::InvalidData, "The request body is too large."));
                }

                body_len
            }
            None => max,
        };

        // Initialize the buffer for saving the body.
        let mut buf = Vec::with_capacity(max);

        // Convert the max value to u64 for the take method call. Should always work on 64bit systems.
        let max: u64 = max.try_into()
            .map_err(|_| Error::new(ErrorKind::Other, "Converting from usize to u64 failed."))?;

        // Create the limited reader.
        let mut reader = AsyncReadExt::take(self, max);

        // Read the body and check that we did not read to much.
        // Can not happen as long as the take implementation is correct, but its is saver to check.
        let amount = AsyncReadExt::read_to_end(&mut reader, &mut buf).await?;
        if amount>buf.len() {
            return Err(Error::new(ErrorKind::Other, "The request body is too large."));
        }

        Ok(buf)
    }

    #[cfg(feature = "json")]
    pub async fn to_json<T: DeserializeOwned>(&mut self, max: usize) -> std::io::Result<T> {
        let buf = self.read_to_vec(max).await?;

        let json: T = serde_json::from_slice(&buf)
            .map_err(|err| {
                log::debug!("Parsing json body failed. err: {:?}", err);

                Error::new(ErrorKind::InvalidData, "Invalid json.")
            })?;

        Ok(json)
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Body")
         .field("has_body", &self.body.is_some())
         .field("body_len", &self.body_len)
         .finish()
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        if self.finish_on_drop {
            if let Some(mut body) = self.body.take() {
                tokio::spawn(async move {
                    let mut sink = tokio::io::sink();
                    if let Err(err) = tokio::io::copy(&mut body, &mut sink).await {
                        log::debug!("Error while finishing body read. err: {:?}", err);
                    }
                });
            }
        }
    }
}

impl AsyncRead for Body {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>
    ) -> Poll<std::io::Result<()>> {
        let inner = Pin::into_inner(self);

        match inner.body.as_mut() {
            Some(body) => Pin::new(body).poll_read(cx, buf),
            None => Poll::Ready(Ok(())),
        }
    }
}
