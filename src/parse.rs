use std::io::{Error, ErrorKind};
use std::convert::TryFrom;

use http::header::{HeaderName, HeaderValue};



pub const CR: u8 = b'\r';
pub const NL: u8 = b'\n';
pub const SP: u8 = b' ';
pub const HTAB: u8 = b'\t';
pub static CRNL: &[u8;2] = b"\r\n";
pub const DQ: u8 = b'"';

pub static HTTP1: &[u8;8] = b"HTTP/1.0";
pub static HTTP1_1: &[u8;8] = b"HTTP/1.1";
pub static HTTP2: &[u8;8] = b"HTTP/2.0";




pub fn is_char(b: u8) -> bool {
    128>b
}

pub fn is_ctl(b: u8) -> bool {
    32>=b || b==127
}

pub fn is_hex(b: u8) -> bool {
    if b>=b'a' && b'f'>=b {
        return true
    }
    if b>=b'A' && b'F'>=b {
        return true
    }
    if b>=b'0' && b'9'>=b {
        return true
    }

    false
}

pub fn is_lws(b: u8) -> bool {
    b==SP || b==HTAB
}

pub fn is_lws_full(b: u8) -> bool {
    b==CR || b==NL || b==SP || b==HTAB
}

pub fn is_text(b: u8) -> bool {
    (b>31 && 127>b) || b>159
}

pub fn is_token(b: u8) -> bool {
    is_char(b) && !is_ctl(b) && !is_seperator(b)
}

pub fn is_seperator(b: u8) -> bool {
    match b {
        b'(' | b')' => true,
        b'<' | b'>' => true,
        b'@' | b',' | b';' | b':' => true,
        b'\\' | DQ | b'/' => true,
        b'[' | b']' => true,
        b'?' | b'=' => true,
        b'{' | b'}' => true,
        SP | HTAB => true,
        _ => false,
    }
}



pub fn find_next_sp(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    let mut idx: usize = 0;

    while buf.len()>idx {
        if buf[idx]==SP {
            return Ok(Some(idx));
        }

        idx = idx+1;
    }

    Ok(None)
}

pub fn find_start_line_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    let mut idx: usize = 0;

    while buf.len()>(idx+1) {
        if buf[idx]==CR {
            if buf[idx+1]==NL {
                return Ok(Some(idx+2));
            } else {
                return Err(Error::new(ErrorKind::InvalidData, "The start line had a CR that was not followed by NL."));
            }
        }

        idx = idx+1;
    }

    Ok(None)
}

pub fn find_header_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    // Check that the line does not start with a CR.
    if buf[0]==CR {
        return Err(Error::new(ErrorKind::InvalidData, "The header started with a CR."));
    }

    let mut idx = 1usize;

    while buf.len()>(idx+2) {
        if buf[idx]==CR {
            if buf[idx+1]==NL {
                if is_lws(buf[idx+2]) {
                    idx = idx+3;
                    continue;
                } else {
                    return Ok(Some(idx+2));
                }
            } else {
                return Err(Error::new(ErrorKind::InvalidData, "A CR was not followed by NL."));
            }
        }

        idx = idx+1;
    }

    // We did not yet find the end. (The line must end with a CRNL)
    Ok(None)
}

pub fn find_lws_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    let mut idx: usize = 0;

    if buf[0]==CR {
        if 3>buf.len() {
            return Ok(None);
        }

        if buf[1]!=NL {
            return Err(Error::new(ErrorKind::InvalidData, "LWS had a CR that was not followed by a NL."));
        }

        if !is_lws(buf[2]) {
            // This is not LWS, but a regular CRNL.
            return Ok(Some(0))
        }

        idx = 3;
    }

    while buf.len()>idx {
        if !is_lws(buf[idx]) {
            return Ok(Some(idx));
        }

        idx = idx+1;
    }

    Ok(None)
}

pub fn find_multi_lws_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    let mut idx: usize = 0;

    while buf.len()>idx {
        match find_lws_end(&buf[idx..])? {
            Some(new_idx) => {
                if new_idx>0 {
                    idx = idx+new_idx;
                } else {
                    break;
                }
            }
            None => return Ok(None),
        }
    }

    Ok(Some(idx))
}

pub fn find_text_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    let mut idx: usize = 0;

    while buf.len()>idx {
        if !is_text(buf[idx]) {
            return Ok(Some(idx));
        }

        idx = idx+1;
    }

    Ok(None)
}

pub fn find_hex_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    let mut idx: usize = 0;

    while buf.len()>idx {
        if !is_hex(buf[idx]) {
            return Ok(Some(idx));
        }

        idx = idx+1;
    }

    Ok(None)
}

pub fn find_token_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    let mut idx: usize = 0;

    while buf.len()>idx {
        if !is_token(buf[idx]) {
            return Ok(Some(idx));
        }

        idx = idx+1;
    }

    Ok(None)
}

pub fn find_quoted_string_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    if buf[0]!=DQ {
        return Err(Error::new(ErrorKind::InvalidData, "Quoted-string did not start with a '\"'."));
    }

    // NOTE text includes all but a few char octets. A quoted-pair contains chars.
    // Given so it does not matter what follows a "\", since afterwards all octets are allowed.

    let mut idx: usize = 0;
    let mut ignore_next = false;

    while buf.len()>idx {
        let b = buf[idx];

        // If the last byte was the escape sequence, ignore this byte.
        if ignore_next {
            idx = idx+1;
            ignore_next = false;
            continue;
        }

        // If this is text without a '"" just continue.
        if is_text(b) && b!=DQ{
            idx = idx+1;
            continue;
        }

        // Check if the quoted-string ended.
        if b==DQ {
            idx = idx+1;
            return Ok(Some(idx));
        }

        return Err(Error::new(ErrorKind::InvalidData, "Invalid quoted-string."));
    }

    Ok(None)
}

pub fn find_chunk_header_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    // Make sure the first letter is a hex one.
    if !is_hex(buf[0]) {
        return Err(Error::new(ErrorKind::InvalidData, "Chunked-Encoding error: First char as not hex!"));
    }

    // Find the chunck-size.
    let mut idx = match find_hex_end(buf)? {
        Some(idx) => idx,
        None => return Ok(None),
    };

    if idx>=buf.len() {
        return Ok(None);
    }

    // Find all extensions
    while buf.len()>idx {
        let b = buf[idx];

        if b==b';' {
            let end = match find_chunk_header_extension_end(&buf[idx..])? {
                Some(end) => end,
                None => return Ok(None),
            };

            idx = idx+end;
            continue;
        }

        if b==CR {
            idx = idx+1;

            if idx>=buf.len() {
                return Ok(None);
            }

            if buf[idx]==NL {
                idx = idx+1;
                return Ok(Some(idx));
            }
        }

        return Err(Error::new(ErrorKind::InvalidData, "Chunked-Encoding error: Invalid chunk header!"));
    }

    Ok(None)
}

pub fn find_chunk_header_extension_end(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }

    // Make sure the first letter is ";".
    if buf[0]!=b';' {
        return Err(Error::new(ErrorKind::InvalidData, "Chunked-Encoding error: chunk-extension did not start with ';'."));
    }

    // Find the chunk-ext-name.
    let idx = match find_token_end(&buf[1..])? {
        Some(idx) => idx,
        None => return Ok(None),
    };

    if (idx+1)>=buf.len() {
        return Ok(None);
    }

    // Make sure the next letter is "=".
    if buf[idx]!=b'=' {
        return Err(Error::new(ErrorKind::InvalidData, "Chunked-Encoding error: chunk-extension did not contain the '='."));
    }

    // Find the chunk-ext-name.
    let idx = if buf[idx+1]==DQ {
        match find_quoted_string_end(&buf[idx+1..])? {
            Some(idx) => idx,
            None => return Ok(None),
        }
    } else {
        match find_token_end(&buf[idx+1..])? {
            Some(idx) => idx,
            None => return Ok(None),
        }
    };

    Ok(Some(idx))
}



pub fn parse_start_line2(buf: &[u8]) -> std::io::Result<(http::Method, http::Uri, http::Version)> {
    if buf.is_empty() {
        return Err(Error::new(ErrorKind::InvalidData, "Start line do not contain a SP."));
    }

    let mut start: usize = 0;

    // Find the method.
    let end = match find_next_sp(buf)? {
        Some(idx) => idx,
        None => return Err(Error::new(ErrorKind::InvalidData, "Start line do not contain a SP.")),
    };

    // Makre sure there is still more data.
    if (end+1)>=buf.len() {
        return Err(Error::new(ErrorKind::InvalidData, "Start line ended early."))
    }

    let method = match http::Method::from_bytes(&buf[start..end]) {
        Ok(method) => method,
        Err(_) => return Err(Error::new(ErrorKind::InvalidData, "Invalid http method.")),
    };
    start = end+1;

    // Find the uri.
    let end = match find_next_sp(&buf[start..])? {
        Some(idx) => start+idx,
        None => return Err(Error::new(ErrorKind::InvalidData, "Start line do not contain a second SP.")),
    };

    // Makre sure there is still more data.
    if (end+8)>=buf.len() {
        return Err(Error::new(ErrorKind::InvalidData, "Start line ended early."))
    }

    // Parse the uri.
    let uri = match http::Uri::try_from(&buf[start..end]) {
        Ok(uri) => uri,
        Err(_) => return Err(Error::new(ErrorKind::InvalidData, "URI was invalid.")),
    };

    start = end+1;

    // Parse the http version.
    let version = &buf[start..buf.len()-2];
    let version = if version==HTTP1 {
        http::Version::HTTP_10
    } else if version==HTTP1_1 {
        http::Version::HTTP_11
    } else if version==HTTP2 {
        http::Version::HTTP_2
    } else {
        log::warn!("Unknown/Invalid HTTP version: {}", String::from_utf8_lossy(version));
        http::Version::HTTP_11
        // return Err(Error::new(ErrorKind::InvalidData, "Unknown/Invalid HTTP version."));
    };

    Ok((method, uri, version))
}

pub fn parse_request_header_new(buf: &[u8]) -> std::io::Result<(HeaderName, Vec<HeaderValue>)> {
    if buf.is_empty() {
        return Err(Error::new(ErrorKind::Other, "Implementation error. (invalid header 1)"));
    }

    // Check that the line starts with a token.
    if !is_token(buf[0]) {
        return Err(Error::new(ErrorKind::Other, "Implementation error. (the header name did not start with a token)"));
    }

    // Find the index where the header name ends.
    let idx = match find_token_end(buf)? {
        Some(idx) => idx,
        None => return Err(Error::new(ErrorKind::Other, "Implementation error. (invalid header 2)")),
    };

    let name = match HeaderName::from_bytes(&buf[..idx]) {
        Ok(name) => name,
        Err(_) => return Err(Error::new(ErrorKind::InvalidData, "The header name was invalid.")),
    };

    // Make sure we can read at least two more bytes (the seperator and the byte after it).
    if (idx+1)>=buf.len() {
        return Err(Error::new(ErrorKind::Other, "Implementation error. (invalid header 3)"));
    }

    if buf[idx]!=b':' {
        return Err(Error::new(ErrorKind::InvalidData, "The header name token was not followed by a ':'."));
    }

    // Find the index where the (optional) LWS ends.
    let mut idx = match find_multi_lws_end(&buf[idx+1..])? {
        Some(new_idx) => idx+new_idx+1,
        None => return Err(Error::new(ErrorKind::Other, "Implementation error. (invalid header 4)")),
    };

    let mut values = Vec::new();

    while buf.len()>idx {
        let b = buf[idx];

        if is_text(b) {
            match find_text_end(&buf[idx..])? {
                Some(end) => {
                    let end = idx+end;
                    let value = match HeaderValue::from_bytes(&buf[idx..end]) {
                        Ok(name) => name,
                        Err(_) => return Err(Error::new(ErrorKind::InvalidData, "The header name was invalid.")),
                    };
                    log::debug!("Detected header content: {}", String::from_utf8_lossy(&buf[idx..end]));
                    values.push(value);
                    idx = end;
                }
                None => return Err(Error::new(ErrorKind::Other, "Implementation error. (invalid header 5)")),
            };
            continue;
        }

        if is_lws_full(b) {
            match find_multi_lws_end(&buf[idx..])? {
                Some(end) => {
                    if end>0 {
                        idx = idx+end;
                    } else {
                        // We found the line end.
                        return Ok((name, values));
                    }
                }
                None => {
                    log::debug!("idx: {}", idx);
                    log::debug!("### {}", String::from_utf8_lossy(&buf[idx..]));
                    return Err(Error::new(ErrorKind::Other, "Implementation error. (invalid header 6)"));
                }
            };
            continue;
        }

        return Err(Error::new(ErrorKind::Other, "Implementation error. (invalid header 7)"));
    }

    Err(Error::new(ErrorKind::Other, "Implementation error. (invalid header 8)"))
}

pub fn parse_hex_byte(b: u8) -> std::io::Result<usize> {
    let num: usize = match b {
        b'0' => 0,
        b'1' => 1,
        b'2' => 2,
        b'3' => 3,
        b'4' => 4,
        b'5' => 5,
        b'6' => 6,
        b'7' => 7,
        b'8' => 8,
        b'9' => 9,
        b'a' | b'A' => 10,
        b'b' | b'B' => 11,
        b'c' | b'C' => 12,
        b'd' | b'D' => 13,
        b'e' | b'E' => 14,
        b'f' | b'F' => 15,
        _ => return Err(Error::new(ErrorKind::InvalidData, "Invalid hex char!")),
    };

    Ok(num)
}

pub fn parse_hex(buf: &[u8]) -> std::io::Result<usize> {
    log::debug!("parse_hex: buf contents: {}", String::from_utf8_lossy(buf));

    if buf.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "Empty buffer."));
    }

    let mut idx = buf.len()-1;
    let mut i = 0;
    let mut total = 0;

    loop {
        let num = parse_hex_byte(buf[idx])?;
        log::debug!("parse_hex: num: {}", num);
        let num = num << i;
        log::debug!("parse_hex: num after shift: {}, i: {}", num, i);

        total = total+num;

        if idx==0 {
            break;
        }

        idx = idx-1;
        i = i+4; // 16 = 2^4
    }

    log::debug!("parse_hex: total: {}", total);

    Ok(total)
}

pub fn parse_chunk_header(buf: &[u8]) -> std::io::Result<usize> {
    log::debug!("parse_chunk_header: buf contents: {}", String::from_utf8_lossy(buf));

    if buf.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "Empty buffer."));
    }

    // Find the chunck-size.
    let idx = match find_hex_end(buf)? {
        Some(idx) => idx,
        None => return Err(Error::new(ErrorKind::Other, "Implementation error.")),
    };

    log::debug!("parse_chunk_header: hex end: {}", idx);

    parse_hex(&buf[..idx])
}
