use std::fmt;
use std::str::FromStr;



/// A single range of bytes.
#[derive(Debug, Clone, Copy)]
pub struct ByteRange {
    pub begin: Option<u64>,
    pub end: Option<u64>,
}

impl fmt::Display for ByteRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let begin = match self.begin {
            Some(byte) => byte.to_string(),
            None => String::new(),
        };

        let end = match self.end {
            Some(byte) => byte.to_string(),
            None => String::new(),
        };

        write!(f, "ByteRange({}-{})", begin, end)
    }
}

/// Multiple ranges of bytes.
pub struct ByteRanges(Vec<ByteRange>);

impl From<Vec<ByteRange>> for ByteRanges {
    fn from(ranges: Vec<ByteRange>) -> Self {
        Self(ranges)
    }
}

impl From<ByteRanges> for Vec<ByteRange> {
    fn from(ranges: ByteRanges) -> Self {
        ranges.into_inner()
    }
}

impl ByteRanges {
    pub fn into_inner(self) -> Vec<ByteRange> {
        self.0
    }
}

impl FromStr for ByteRanges {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(());
        }

        let ranges: Vec<&str> = match s.strip_prefix("bytes=") {
            Some(s) => s.split(",").collect(),
            None => return Err(()),
        };

        let mut store = Vec::with_capacity(ranges.len());

        for range in ranges.iter() {
            if range.is_empty() {
                return Err(());
            }

            let parts: Vec<&str> = range.split("-").collect();

            if parts.len()!=2 {
                return Err(());
            }

            let begin = match parts[0].parse::<u64>() {
                Ok(begin) => Some(begin),
                Err(err) => {
                    log::debug!("Error while parsing the Range headers first number: {:?}, full: {}", err, range);
                    return Err(());
                }
            };

            let end = if parts[1].is_empty() {
                None
            } else {
                match parts[1].parse::<u64>() {
                    Ok(end) => Some(end),
                    Err(err) => {
                        log::debug!("Error while parsing the Range headers second number: {:?}, full: {}", err, range);
                        return Err(());
                    }
                }
            };

            if let Some(end) = &end {
                if let Some(begin) = &begin {
                    if *begin>*end {
                        log::debug!("Error while parsing the Range header. begin is higher then end! begin: {}, end: {}", begin, end);
                        return Err(());
                    }
                }
            }

            store.push(ByteRange {
                begin,
                end,
            });
        }

        Ok(store.into())
    }
}
