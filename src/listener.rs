use crate::*;

use std::path::{Path, PathBuf};



/// A listener which accepts connections.
#[async_trait]
pub trait Listener: Send + Unpin {
    type Conn: AsyncRead + AsyncWrite + Send + Unpin;

    async fn accept(&self) -> std::io::Result<(Self::Conn, Extensions)>;
}

/// Negotiates on an already accepted connection. Mostly usefull for offering TLS.
pub trait Negotiator<IO, Conn>: Send + Unpin where
    IO: AsyncRead + AsyncWrite + Send + Unpin,
    Conn: AsyncRead + AsyncWrite + Send + Unpin,
{
    fn negotiate<'life0, 'life1, 'async_trait>(
        &self,
        conn: IO,
        ext: &'life1 mut Extensions,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Conn>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
        IO: 'async_trait;
}


#[async_trait]
impl<T: Listener + Sync> Listener for Box<T> {
    type Conn = <T as Listener>::Conn;

    async fn accept(&self) -> std::io::Result<(Self::Conn, Extensions)> {
        Listener::accept(self.as_ref()).await
    }
}

impl<IO, Conn, T: Negotiator<IO, Conn>> Negotiator<IO, Conn> for Box<T> where
    IO: AsyncRead + AsyncWrite + Send + Unpin,
    Conn: AsyncRead + AsyncWrite + Send + Unpin,
{
    fn negotiate<'life0, 'life1, 'async_trait>(
        &self,
        conn: IO,
        ext: &'life1 mut Extensions,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Conn>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
        IO: 'async_trait,
    {
        Negotiator::negotiate(self.as_ref(), conn, ext)
    }
}


#[async_trait]
impl Listener for tokio::net::TcpListener {
    type Conn = tokio::net::TcpStream;

    async fn accept(&self) -> std::io::Result<(Self::Conn, Extensions)> {
        let (conn, addr) = self.accept().await?;
        let mut ext = Extensions::default();

        let mut info = ConnectionInfo::default();
        info.peer_addr = Some(addr);

        ext.insert(info);

        Ok((conn, ext))
    }
}

#[async_trait]
impl Listener for tokio::net::UnixListener {
    type Conn = tokio::net::UnixStream;

    async fn accept(&self) -> std::io::Result<(Self::Conn, Extensions)> {
        let (conn, addr) = self.accept().await?;
        let mut ext = Extensions::default();

        let mut info = ConnectionInfo::default();
        info.unix_info = Some(addr.into());
        if let Ok(cred) = conn.peer_cred() {
            info.peer_cred = Some(cred);
        }

        ext.insert(info);

        Ok((conn, ext))
    }
}


pub(crate) struct NoNegotiator {}

impl<IO> Negotiator<IO, IO> for NoNegotiator where
    IO: AsyncRead + AsyncWrite + Send + Unpin,
{
    fn negotiate<'life0, 'life1, 'async_trait>(
        &self,
        conn: IO,
        _ext: &'life1 mut Extensions,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<IO>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
        IO: 'async_trait,
    {
        Box::pin(async move {
            Ok(conn)
        })
    }
}

#[cfg(feature = "tokio-rustls")]
impl<IO> Negotiator<IO, tokio_rustls::server::TlsStream<IO>> for tokio_rustls::TlsAcceptor where
    IO: AsyncRead + AsyncWrite + Send + Unpin,
{
    fn negotiate<'life0, 'life1, 'async_trait>(
        &self,
        conn: IO,
        ext: &'life1 mut Extensions,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<tokio_rustls::server::TlsStream<IO>>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
        IO: 'async_trait,
    {
        let conn = self.accept(conn);

        Box::pin(async move {
            let conn = conn.await?;

            // Determine the ALPN protocol.
            let (_, session) = conn.get_ref();
            if let Some(buf) = tokio_rustls::rustls::Session::get_alpn_protocol(session) {
                let protocol = String::from_utf8_lossy(buf).into_owned();

                if let Some(info) = ext.get_mut::<ConnectionInfo>() {
                    info.alpn_protocol = Some(protocol);
                }
            }

            Ok(conn)
        })
    }
}


/// Contains information on a specific connection, like the remote peer`s address.
#[derive(Default, Debug)]
pub struct ConnectionInfo {
    /// The remote peer's socket address.
    pub peer_addr: Option<std::net::SocketAddr>,
    /// The local unix socket info of the connection.
    pub unix_info: Option<UnixInfo>,
    /// The remote credentials for a unix socket connection.
    pub peer_cred: Option<tokio::net::unix::UCred>,
    /// The protocol agreed on over the TLS ALPN extension. Usefull for HTTP/2.
    pub alpn_protocol: Option<String>,
}

impl ConnectionInfo {
    /// Was the HTTP/2 protocol negotiated over ALPN?
    pub fn is_h2(&self) -> bool {
        if let Some(alpn) = &self.alpn_protocol {
            if alpn=="h2" {
                return true
            }
        }

        false
    }
}

/// Contains information about an unixsocket connection. Part of [`ConnectionInfo`].
#[derive(Default, Debug, Clone)]
pub struct UnixInfo {
    pub is_unnamed: bool,
    pub as_pathname: Option<PathBuf>,
}

impl From<tokio::net::unix::SocketAddr> for UnixInfo {
    fn from(addr: tokio::net::unix::SocketAddr) -> Self {
        Self {
            is_unnamed: addr.is_unnamed(),
            as_pathname: addr.as_pathname().map(|path| path.to_path_buf()),
        }
    }
}

impl UnixInfo {
    pub fn is_unnamed(&self) -> bool {
        self.is_unnamed
    }

    pub fn as_pathname(&self) -> Option<&Path> {
        match self.as_pathname.as_ref() {
            Some(path) => Some(path.as_ref()),
            None => None
        }
    }
}
