use num_derive::FromPrimitive;



pub type H2Result<T> = Result<T, H2Error>;

#[derive(Debug, Clone, Copy)]
pub enum H2Error {
    /// The error is related only to the stream where it occured.
    Stream(ErrorCode),
    /// The error affects the entire connection.
    Connection(ErrorCode),
}

impl H2Error {
    pub fn into_inner(self) -> ErrorCode {
        match self {
            Self::Stream(code) => code,
            Self::Connection(code) => code,
        }
    }
}

impl From<std::io::Error> for H2Error {
    fn from(_err: std::io::Error) -> Self {
        Self::Connection(ErrorCode::ProtocolError)
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy)]
#[derive(FromPrimitive)]
pub enum ErrorCode {
    /// The associated condition is not a result of an error. For example, a GOAWAY might include
    /// this code to indicate graceful shutdown of a connection.
    NoError = 0,
    /// The endpoint detected an unspecific protocol error. This error is for use when a more
    /// specific error code is not available.
    ProtocolError = 1,
    /// The endpoint encountered an unexpected internal error.
    InternalError = 2,
    /// The endpoint detected that its peer violated the flow-control protocol.
    FlowControlError = 3,
    /// The endpoint sent a SETTINGS frame but did not receive a response in a timely manner.
    SettingsTimeout = 4,
    /// The endpoint received a frame after a stream was half-closed.
    StreamClosed = 5,
    /// The endpoint received a frame with an invalid size.
    FrameSizeError = 6,
    /// The endpoint refused the stream prior to performing any application processing.
    RefusedStream = 7,
    /// Used by the endpoint to indicate that the stream is no longer needed.
    Cancel = 8,
    /// The endpoint is unable to maintain the header compression context for the connection.
    CompresssionError = 9,
    /// The connection established in response to a CONNECT request was reset or abnormally closed.
    ConnectError = 10,
    /// The endpoint detected that its peer is exhibiting a behavior that might be generating excessive load.
    EnhanceYourCalm = 11,
    /// The underlying transport has properties that do not meet minimum security requirements.
    InadequateSecurity = 12,
    /// The endpoint requires that HTTP/1.1 be used instead of HTTP/2.
    Http11Required = 13,
}
