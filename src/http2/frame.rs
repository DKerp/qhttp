use super::*;



#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(FromPrimitive)]
pub enum FrameType {
    Data = 0,
    Headers = 1,
    Priority = 2,
    RstStream = 3,
    Settings = 4,
    PushPromise = 5,
    Ping = 6,
    GoAway = 7,
    WindowUpdate = 8,
    Continuation = 9,
}

bitflags! {
    pub struct Flags: u8 {
        const END_STREAM = 1;
        const ACK = 1;
        const END_HEADERS = 1<<2;
        const PADDED = 1<<3;
        const PRIORITY = 1<<5;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Priority {
    pub stream_dependency: u32,
    pub exclusive: bool,
    pub weight: u8,
}

impl From<&[u8]> for Priority {
    fn from(slice: &[u8]) -> Self {
        let stream_dependency: [u8; 4] = slice[0..4].try_into().unwrap();
        let stream_dependency = u32::from_be_bytes(stream_dependency);
        log::debug!("stream_dependency: {}", stream_dependency);
        let (stream_dependency, exclusive) = if stream_dependency>=(1<<31) {
            (stream_dependency-(1<<31), true)
        } else {
            (stream_dependency, false)
        };
        log::debug!("stream_dependency: {}, exclusive: {}", stream_dependency, exclusive);
        let weight = slice[4];

        Self {
            stream_dependency,
            exclusive,
            weight,
        }
    }
}

impl From<Priority> for Vec<u8> {
    fn from(priority: Priority) -> Self {
        let mut payload = Vec::with_capacity(5);
        let stream_dependency = if priority.exclusive {
            priority.stream_dependency | (1<<31)
        } else {
            priority.stream_dependency & !(1<<31)
        };
        payload.extend_from_slice(&stream_dependency.to_be_bytes());
        payload.push(priority.weight);

        payload
    }
}


#[derive(Debug, Clone)]
pub struct DataFrame {
    pub stream: u32,
    pub end_stream: bool,
    pub data: Vec<u8>,
}

impl TryFrom<Frame> for DataFrame {
    type Error = H2Error;

    fn try_from(frame: Frame) -> H2Result<Self> {
        if frame.header.r#type!=FrameType::Data {
            log::error!("You can not convert {:?} into a data frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        if frame.header.stream==0 {
            return Err(H2Error::Connection(ErrorCode::ProtocolError));
        }

        Ok(Self {
            stream: frame.header.stream,
            end_stream: frame.header.flags.contains(Flags::END_STREAM),
            data: frame.payload,
        })
    }
}

impl From<DataFrame> for Frame {
    fn from(frame: DataFrame) -> Self {
        let r#type = FrameType::Data;
        let mut flags = Flags::empty();
        if frame.end_stream {
            flags = flags.union(Flags::END_STREAM);
        }
        let stream = frame.stream;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };

        let payload = frame.data;

        Self {
            header,
            payload,
        }
    }
}


#[derive(Debug, Clone)]
pub struct HeadersFrame {
    pub stream: u32,
    pub end_stream: bool,
    pub end_headers: bool,
    pub priority: Option<Priority>,
    pub data: Vec<u8>,
}

impl TryFrom<Frame> for HeadersFrame {
    type Error = H2Error;

    fn try_from(mut frame: Frame) -> H2Result<Self> {
        if frame.header.r#type!=FrameType::Headers {
            log::error!("You can not convert {:?} into a headers frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        if frame.header.stream==0 {
            return Err(H2Error::Connection(ErrorCode::ProtocolError));
        }

        let padded = frame.header.flags.contains(Flags::PADDED);

        let priority = if frame.header.flags.contains(Flags::PRIORITY) {
            if padded {
                let priority: Priority = (frame.payload[8..13]).into();

                frame.payload = frame.payload.split_off(13);

                Some(priority)
            } else {
                let priority: Priority = (frame.payload[0..5]).into();

                frame.payload = frame.payload.split_off(5);

                Some(priority)
            }
        } else {
            None
        };

        Ok(Self {
            stream: frame.header.stream,
            end_stream: frame.header.flags.contains(Flags::END_STREAM),
            end_headers: frame.header.flags.contains(Flags::END_HEADERS),
            priority,
            data: frame.payload,
        })
    }
}

impl From<HeadersFrame> for Frame {
    fn from(frame: HeadersFrame) -> Self {
        let r#type = FrameType::Headers;
        let mut flags = Flags::empty();
        if frame.end_stream {
            flags = flags.union(Flags::END_STREAM);
        }
        if frame.end_headers {
            flags = flags.union(Flags::END_HEADERS);
        }
        let stream = frame.stream;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };

        let payload = match frame.priority {
            Some(priority) => {
                let mut payload = vec![0u8; 5+frame.data.len()];
                let priority: Vec<u8> = priority.into();
                payload[..5].copy_from_slice(&priority[..]);
                payload[5..].copy_from_slice(&frame.data[..]);

                payload
            }
            None => frame.data,
        };

        Self {
            header,
            payload,
        }
    }
}


#[derive(Debug, Clone, Copy)]
pub struct PriorityFrame {
    pub stream: u32,
    pub priority: Priority,
}

impl TryFrom<Frame> for PriorityFrame {
    type Error = H2Error;

    fn try_from(frame: Frame) -> H2Result<Self> {
        if frame.header.r#type!=FrameType::Priority {
            log::error!("You can not convert {:?} into a priority frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        let priority: Priority = frame.payload[..].into();

        Ok(Self {
            stream: frame.header.stream,
            priority,
        })
    }
}

impl From<PriorityFrame> for Frame {
    fn from(frame: PriorityFrame) -> Self {
        let r#type = FrameType::Priority;
        let flags = Flags::empty();
        let stream = frame.stream;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };
        let payload = frame.priority.into();

        Self {
            header,
            payload,
        }
    }
}


#[derive(Debug, Clone, Copy)]
pub struct RstStreamFrame {
    pub stream: u32,
    pub error_code: Option<ErrorCode>,
}

impl TryFrom<Frame> for RstStreamFrame {
    type Error = H2Error;

    fn try_from(frame: Frame) -> H2Result<Self> {
        if frame.header.r#type!=FrameType::RstStream {
            log::error!("You can not convert {:?} into a reset stream frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        let error_code: [u8; 4] = frame.payload[..].try_into().unwrap();
        let error_code = u32::from_be_bytes(error_code);
        let error_code = ErrorCode::from_u32(error_code);

        Ok(Self {
            stream: frame.header.stream,
            error_code,
        })
    }
}

impl From<RstStreamFrame> for Frame {
    fn from(frame: RstStreamFrame) -> Self {
        let r#type = FrameType::RstStream;
        let flags = Flags::empty();
        let stream = frame.stream;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };

        let error_code: u32 = match frame.error_code {
            Some(code) => code as u32,
            None => ErrorCode::InternalError as u32,
        };
        let payload = error_code.to_be_bytes().to_vec();

        Self {
            header,
            payload,
        }
    }
}


#[derive(Default, Debug, Clone, Copy)]
pub struct SettingsFrame {
    pub ack: bool,
    pub header_table_size: Option<u32>,
    pub enable_push: Option<bool>,
    pub max_concurrent_streams: Option<u32>,
    pub initial_window_size: Option<u32>,
    pub max_frame_size: Option<u32>,
    pub max_header_list_size: Option<u32>,
}

impl From<Settings> for SettingsFrame {
    fn from(settings: Settings) -> Self {
        Self {
            ack: false,
            header_table_size: Some(settings.header_table_size),
            enable_push: Some(settings.enable_push),
            max_concurrent_streams: settings.max_concurrent_streams,
            initial_window_size: Some(settings.initial_window_size),
            max_frame_size: Some(settings.max_frame_size),
            max_header_list_size: settings.max_header_list_size,
        }
    }
}

impl TryFrom<Frame> for SettingsFrame {
    type Error = H2Error;

    fn try_from(frame: Frame) -> Result<Self, Self::Error> {
        if frame.header.r#type!=FrameType::Settings {
            log::error!("You can not convert {:?} into a settings frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        let mut settings_frame = Self::default();

        settings_frame.ack = frame.header.flags.contains(Flags::ACK);
        if settings_frame.ack {
            if !frame.payload.is_empty() {
                return Err(H2Error::Connection(ErrorCode::FrameSizeError));
            }

            return Ok(settings_frame);
        }

        // We only parse each settings once. Each unknown setting and each double setting
        // gets treated as a connection error of type protocol error.
        for chunk in frame.payload.chunks_exact(6) {
            let ident: [u8; 2] = chunk[..2].try_into().map_err(|_| H2Error::Connection(ErrorCode::InternalError))?;
            log::debug!("Parsing SettingsFrame - got ident: {:?}", ident);
            let ident = u16::from_be_bytes(ident);
            let ident = SettingsIdent::from_u16(ident).ok_or_else(|| H2Error::Connection(ErrorCode::ProtocolError))?;
            log::debug!("Parsing SettingsFrame - got ident: {:?}", ident);

            let value: [u8; 4] = chunk[2..6].try_into().map_err(|_| H2Error::Connection(ErrorCode::InternalError))?;
            log::debug!("Parsing SettingsFrame - got value: {:?}", ident);
            let value = u32::from_be_bytes(value);

            match ident {
                SettingsIdent::HeaderTableSize => {
                    if settings_frame.header_table_size.is_some() {
                        log::debug!("Parsing SettingsFrame - header table size was send twice!");
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }

                    settings_frame.header_table_size = Some(value);
                }
                SettingsIdent::EnablePush => {
                    if settings_frame.enable_push.is_some() {
                        log::debug!("Parsing SettingsFrame - enable push was send twice!");
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }

                    match value {
                        1 => settings_frame.enable_push = Some(true),
                        0 => settings_frame.enable_push = Some(false),
                        _ => return Err(H2Error::Connection(ErrorCode::ProtocolError)),
                    }
                }
                SettingsIdent::MaxConcurrentStreams => {
                    if settings_frame.max_concurrent_streams.is_some() {
                        log::debug!("Parsing SettingsFrame - max concurrent streams was send twice!");
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }

                    settings_frame.max_concurrent_streams = Some(value);
                }
                SettingsIdent::InitialWindowSize => {
                    if settings_frame.initial_window_size.is_some() {
                        log::debug!("Parsing SettingsFrame - initial window size was send twice!");
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }

                    settings_frame.initial_window_size = Some(value);
                }
                SettingsIdent::MaxFrameSize => {
                    if settings_frame.max_frame_size.is_some() {
                        log::debug!("Parsing SettingsFrame - max frame size was send twice!");
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }

                    settings_frame.max_frame_size = Some(value);
                }
                SettingsIdent::MaxHeaderListSize => {
                    if settings_frame.max_header_list_size.is_some() {
                        log::debug!("Parsing SettingsFrame - max header list size was send twice!");
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }

                    settings_frame.max_header_list_size = Some(value);
                }
            }
        }

        Ok(settings_frame)
    }
}

impl From<SettingsFrame> for Frame {
    fn from(frame: SettingsFrame) -> Self {
        let r#type = FrameType::Settings;
        let mut flags = Flags::empty();
        if frame.ack {
            flags = flags.union(Flags::ACK);
        }
        let stream = 0;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };

        let mut payload = Vec::with_capacity(36);
        if let Some(value) = frame.header_table_size {
            payload.extend_from_slice(&(SettingsIdent::HeaderTableSize as u16).to_be_bytes());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        if let Some(value) = frame.enable_push {
            payload.extend_from_slice(&(SettingsIdent::EnablePush as u16).to_be_bytes());
            if value {
                payload.extend_from_slice(&1u32.to_be_bytes());
            } else {
                payload.extend_from_slice(&0u32.to_be_bytes());
            }
        }
        if let Some(value) = frame.max_concurrent_streams {
            payload.extend_from_slice(&(SettingsIdent::MaxConcurrentStreams as u16).to_be_bytes());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        if let Some(value) = frame.initial_window_size {
            payload.extend_from_slice(&(SettingsIdent::InitialWindowSize as u16).to_be_bytes());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        if let Some(value) = frame.max_frame_size {
            payload.extend_from_slice(&(SettingsIdent::MaxFrameSize as u16).to_be_bytes());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        if let Some(value) = frame.max_header_list_size {
            payload.extend_from_slice(&(SettingsIdent::MaxHeaderListSize as u16).to_be_bytes());
            payload.extend_from_slice(&value.to_be_bytes());
        }

        Self {
            header,
            payload,
        }
    }
}


#[derive(Debug, Clone, Copy)]
pub struct PingFrame {
    pub ack: bool,
    pub data: u64,
}

impl TryFrom<Frame> for PingFrame {
    type Error = H2Error;

    fn try_from(frame: Frame) -> H2Result<Self> {
        if frame.header.r#type!=FrameType::Ping {
            log::error!("You can not convert {:?} into a ping frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        let ack = frame.header.flags.contains(Flags::ACK);

        let data: [u8; 8] = frame.payload[..].try_into().unwrap();
        let data = u64::from_be_bytes(data);

        Ok(Self {
            ack,
            data,
        })
    }
}

impl From<PingFrame> for Frame {
    fn from(frame: PingFrame) -> Self {
        let r#type = FrameType::Ping;
        let mut flags = Flags::empty();
        if frame.ack {
            flags = flags.union(Flags::ACK);
        }
        let stream = 0;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };

        let payload = frame.data.to_be_bytes().to_vec();

        Self {
            header,
            payload,
        }
    }
}


#[derive(Debug, Clone)]
pub struct GoAwayFrame {
    pub last_stream_id: u32,
    pub error_code: Option<ErrorCode>,
    pub debug_data: Vec<u8>,
}

impl TryFrom<Frame> for GoAwayFrame {
    type Error = H2Error;

    fn try_from(mut frame: Frame) -> H2Result<Self> {
        if frame.header.r#type!=FrameType::GoAway {
            log::error!("You can not convert {:?} into a go away frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        let last_stream_id: [u8; 4] = frame.payload[0..4].try_into().unwrap();
        let last_stream_id = u32::from_be_bytes(last_stream_id);
        let last_stream_id = if last_stream_id>=(1<<31) {
            last_stream_id-(1<<31)
        } else {
            last_stream_id
        };

        let error_code: [u8; 4] = frame.payload[4..8].try_into().unwrap();
        let error_code = u32::from_be_bytes(error_code);
        let error_code = ErrorCode::from_u32(error_code);

        let debug_data = frame.payload.split_off(8);

        Ok(Self {
            last_stream_id,
            error_code,
            debug_data,
        })
    }
}

impl From<GoAwayFrame> for Frame {
    fn from(frame: GoAwayFrame) -> Self {
        let r#type = FrameType::GoAway;
        let flags = Flags::empty();
        let stream = 0;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };

        let mut payload = Vec::with_capacity(8+frame.debug_data.len());
        payload.extend_from_slice(&frame.last_stream_id.to_be_bytes());
        let error_code: u32 = match frame.error_code {
            Some(code) => code as u32,
            None => ErrorCode::InternalError as u32,
        };
        payload.extend_from_slice(&error_code.to_be_bytes());
        payload.extend_from_slice(&frame.debug_data[..]);

        Self {
            header,
            payload,
        }
    }
}


#[derive(Debug, Clone, Copy)]
pub struct WindowUpdateFrame {
    pub stream: u32,
    pub window_size_increment: u32,
}

impl TryFrom<Frame> for WindowUpdateFrame {
    type Error = H2Error;

    fn try_from(frame: Frame) -> H2Result<Self> {
        if frame.header.r#type!=FrameType::WindowUpdate {
            log::error!("You can not convert {:?} into a window update frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        let window_size_increment: [u8; 4] = frame.payload[0..4].try_into().unwrap();
        let window_size_increment = u32::from_be_bytes(window_size_increment);
        let window_size_increment = if window_size_increment>=(1<<31) {
            window_size_increment-(1<<31)
        } else {
            window_size_increment
        };

        Ok(Self {
            stream: frame.header.stream,
            window_size_increment,
        })
    }
}

impl From<WindowUpdateFrame> for Frame {
    fn from(frame: WindowUpdateFrame) -> Self {
        let r#type = FrameType::WindowUpdate;
        let flags = Flags::empty();
        let stream = frame.stream;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };

        let payload = frame.window_size_increment.to_be_bytes().to_vec();

        Self {
            header,
            payload,
        }
    }
}


#[derive(Debug, Clone)]
pub struct ContinuationFrame {
    pub stream: u32,
    pub end_headers: bool,
    pub data: Vec<u8>,
}

impl TryFrom<Frame> for ContinuationFrame {
    type Error = H2Error;

    fn try_from(frame: Frame) -> H2Result<Self> {
        if frame.header.r#type!=FrameType::Continuation {
            log::error!("You can not convert {:?} into a continuation frame!", frame.header.r#type);
            return Err(H2Error::Connection(ErrorCode::InternalError));
        }

        let end_headers = frame.header.flags.contains(Flags::END_HEADERS);

        Ok(Self {
            stream: frame.header.stream,
            end_headers,
            data: frame.payload,
        })
    }
}

impl From<ContinuationFrame> for Frame {
    fn from(frame: ContinuationFrame) -> Self {
        let r#type = FrameType::Continuation;
        let mut flags = Flags::empty();
        if frame.end_headers {
            flags = flags.union(Flags::END_HEADERS);
        }
        let stream = frame.stream;

        let header = FrameHeader {
            r#type,
            flags,
            stream,
        };

        let payload = frame.data;

        Self {
            header,
            payload,
        }
    }
}



#[derive(Debug, Clone)]
pub struct Frame {
    pub(crate) header: FrameHeader,
    pub(crate) payload: Vec<u8>,
}

impl Frame {
    pub fn from_header_and_payload(
        header: FrameHeaderRaw,
        payload: &[u8],
    ) -> H2Result<Self> {
        let header: FrameHeader = header.try_into()?;

        let mut begin = 0usize;
        let mut end = payload.len();

        match header.r#type {
            FrameType::Data | FrameType::Headers => {
                if header.flags.contains(Flags::PADDED) {
                    begin = begin+1;
                    let padding = payload.get(0).ok_or_else(|| H2Error::Connection(ErrorCode::InternalError))?;
                    let padding = *padding as usize;
                    if padding>=payload.len() {
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }
                    end = end.checked_sub(padding).ok_or_else(|| H2Error::Connection(ErrorCode::ProtocolError))?;
                }
            }
            FrameType::PushPromise => return Err(H2Error::Connection(ErrorCode::ProtocolError)),
            _ => (),
        };

        if begin>end {
            return Err(H2Error::Connection(ErrorCode::ProtocolError))?;
        }

        // TODO get a vec from the pool for this.
        let payload = payload[begin..end].to_vec();

        Ok(Self {
            header,
            payload,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub r#type: FrameType,
    pub flags: Flags,
    pub stream: u32,
}

impl TryFrom<FrameHeaderRaw> for FrameHeader {
    type Error = H2Error;

    fn try_from(raw: FrameHeaderRaw) -> Result<Self, Self::Error> {
        // TODO For now we treat unknown frames as a connection error, even trough the spec says
        // they should be ignored in order to maintain interoperability with extended clients.
        // But lets be honest, are they any out there? And even if so, will they talk to us?
        let r#type = FrameType::from_u8(raw.r#type).ok_or_else(|| H2Error::Connection(ErrorCode::InternalError))?;
        let flags = Flags::from_bits_truncate(raw.flags); // We innore (=unset) unknown flags.
        let stream = raw.stream;

        // Perform all standard validity checks which can be performed without analysing the
        // payload any further (like e.g. padding etc.).
        match r#type {
            FrameType::Data | FrameType::Headers => {
                // Make sure the frame is associated with a stream.
                if stream==0 {
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }

                // Make sure that if there is padding, the payload is big enough.
                let mut min = 0u32;
                if flags.contains(Flags::PADDED) {
                    min = min+1;
                    if min>raw.length {
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }
                }

                // If we have a headers frame, also check that priority data is presend, if any.
                if let FrameType::Headers = r#type {
                    if flags.contains(Flags::PRIORITY) {
                        min = min+5;
                    }
                    if min>raw.length {
                        return Err(H2Error::Connection(ErrorCode::ProtocolError));
                    }
                }
            }
            FrameType::Priority => {
                // Make sure the frame is associated with a stream.
                if stream==0 {
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }

                if raw.length!=5 {
                    return Err(H2Error::Stream(ErrorCode::FrameSizeError));
                }
            }
            FrameType::RstStream => {
                // Make sure the frame is associated with a stream.
                if stream==0 {
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }

                if raw.length!=4 {
                    return Err(H2Error::Connection(ErrorCode::FrameSizeError));
                }
            }
            FrameType::Settings => {
                // Make sure the frame is associated with the connection.
                if stream!=0 {
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }

                // Settings frames have a payload that are a multiple of 6.
                if raw.length%6!=0 {
                    return Err(H2Error::Connection(ErrorCode::FrameSizeError));
                }
            }
            FrameType::PushPromise => {
                // We only call this function to parse frames received by client, but clients never
                // send this frame.
                return Err(H2Error::Connection(ErrorCode::ProtocolError));
            }
            FrameType::Ping => {
                // Make sure the frame is associated with the connection.
                if stream!=0 {
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }

                if raw.length!=8 {
                    return Err(H2Error::Connection(ErrorCode::FrameSizeError));
                }
            }
            FrameType::GoAway => {
                // Make sure the frame is associated with the connection.
                if stream!=0 {
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }

                if 8>raw.length {
                    return Err(H2Error::Connection(ErrorCode::FrameSizeError));
                }
            }
            FrameType::WindowUpdate => {
                if raw.length!=4 {
                    return Err(H2Error::Connection(ErrorCode::FrameSizeError));
                }
            }
            FrameType::Continuation => {
                // Make sure the frame is associated with the connection.
                if stream!=0 {
                    return Err(H2Error::Connection(ErrorCode::ProtocolError));
                }
            }
        }

        Ok(Self {
            r#type,
            flags,
            stream,
        })
    }
}

pub struct FrameHeaderRaw {
    pub length: u32,
    pub r#type: u8,
    pub flags: u8,
    pub stream: u32,
}

impl FrameHeaderRaw {
    const STREAM_ID_LIMIT: u32 = 1<<31;

    pub fn from_slice(buf: &[u8]) -> H2Result<Self> {
        if 9>buf.len() {
            log::warn!("A H2 frame header is exactly 9 bytes long (short input).");
            // return Err(Error::new(ErrorKind::InvalidInput, "A H2 frame header is exactly 9 bytes long (short input)."));
            return Err(H2Error::Connection(ErrorCode::ProtocolError));
        }

        let length = u32::from_be_bytes([0, buf[0], buf[1], buf[2]]); // Immitade an u24.
        let r#type = buf[3];
        let flags = buf[4];
        let stream: [u8; 4] = match buf[5..9].try_into() {
            Ok(stream) => stream,
            Err(err) => {
                log::warn!("Could not convert a byte slice to an u32 stream id. err: {:?}", err);
                return Err(H2Error::Connection(ErrorCode::InternalError));
            }
        };
        let stream = u32::from_be_bytes(stream);

        // The spec says we should ignore the first bit.
        let stream = if stream>=Self::STREAM_ID_LIMIT {
            stream-Self::STREAM_ID_LIMIT
        } else {
            stream
        };

        Ok(Self {
            length,
            r#type,
            flags,
            stream,
        })
    }
}
