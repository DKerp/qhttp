use super::*;



pub static MAX_FRAME_SIZE_MIN: u32 = 16_384;
pub static MAX_FRAME_SIZE_MAX: u32 = 16_777_215;

pub static WINDOW_SIZE_MAX: u32 = (1<<31)-1;



#[repr(u16)]
#[derive(Debug, Clone, Copy)]
#[derive(FromPrimitive)]
pub enum SettingsIdent {
    HeaderTableSize = 1,
    EnablePush = 2,
    MaxConcurrentStreams = 3,
    InitialWindowSize = 4,
    MaxFrameSize = 5,
    MaxHeaderListSize = 6,
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub header_table_size: u32,
    pub enable_push: bool,
    pub max_concurrent_streams: Option<u32>,
    pub initial_window_size: u32,
    pub max_frame_size: u32,
    pub max_header_list_size: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            header_table_size: 4096,
            enable_push: true,
            max_concurrent_streams: None,
            initial_window_size: 65_535,
            max_frame_size: 16_384,
            max_header_list_size: None,
        }
    }
}

impl Settings {
    /// Applies a settings frame and returns the difference in the initial window size.
    pub fn apply_settings_frame(&mut self, frame: SettingsFrame) -> H2Result<i32> {
        if let Some(value) = frame.header_table_size {
            self.set_header_table_size(value)?;
        }
        if let Some(value) = frame.enable_push {
            self.set_enable_push(value)?;
        }
        if let Some(value) = frame.max_concurrent_streams {
            self.set_max_concurrent_streams(value)?;
        }
        if let Some(value) = frame.max_frame_size {
            self.set_max_frame_size(value)?;
        }
        if let Some(value) = frame.max_header_list_size {
            self.set_max_header_list_size(value)?;
        }
        let old: i32 = self.initial_window_size.try_into().map_err(|_| H2Error::Connection(ErrorCode::InternalError))?;
        if let Some(value) = frame.initial_window_size {
            self.set_initial_window_size(value)?;
        }
        let new: i32 = self.initial_window_size.try_into().map_err(|_| H2Error::Connection(ErrorCode::InternalError))?;
        let dif = new.checked_sub(old).ok_or_else(|| H2Error::Connection(ErrorCode::ProtocolError))?;

        Ok(dif)
    }

    pub fn get_header_table_size(&self) -> u32 {
        self.header_table_size
    }

    pub fn set_header_table_size(&mut self, value: u32) -> H2Result<()> {
        self.header_table_size = value;

        Ok(())
    }

    pub fn get_enable_push(&self) -> bool {
        self.enable_push
    }

    pub fn set_enable_push(&mut self, value: bool) -> H2Result<()> {
        self.enable_push = value;

        Ok(())
    }

    pub fn get_max_concurrent_streams(&self) -> Option<u32> {
        self.max_concurrent_streams
    }

    pub fn set_max_concurrent_streams(&mut self, value: u32) -> H2Result<()> {
        self.max_concurrent_streams = Some(value);

        Ok(())
    }

    pub fn get_initial_window_size(&self) -> u32 {
        self.initial_window_size
    }

    pub fn set_initial_window_size(&mut self, value: u32) -> H2Result<()> {
        if value>WINDOW_SIZE_MAX {
            return Err(H2Error::Connection(ErrorCode::FlowControlError));
        }

        self.initial_window_size = value;

        Ok(())
    }

    pub fn get_max_frame_size(&self) -> u32 {
        self.max_frame_size
    }

    pub fn set_max_frame_size(&mut self, value: u32) -> H2Result<()> {
        if MAX_FRAME_SIZE_MIN>value || value>MAX_FRAME_SIZE_MAX {
            return Err(H2Error::Connection(ErrorCode::ProtocolError));
        }

        self.max_frame_size = value;

        Ok(())
    }

    pub fn get_max_header_list_size(&self) -> Option<u32> {
        self.max_header_list_size
    }

    pub fn set_max_header_list_size(&mut self, value: u32) -> H2Result<()> {
        self.max_header_list_size = Some(value);

        Ok(())
    }
}
