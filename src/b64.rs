//! Minimal standard base64 (RFC 4648, with padding), so intent JSON can
//! carry inline `content_b64` payloads without another dependency.

use crate::{Error, Result};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as standard base64 with padding.
pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        out.push(ALPHABET[idx[0] as usize] as char);
        out.push(ALPHABET[idx[1] as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[idx[2] as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[idx[3] as usize] as char
        } else {
            '='
        });
    }
    out
}

fn value_of(c: u8) -> Result<u32> {
    match c {
        b'A'..=b'Z' => Ok((c - b'A') as u32),
        b'a'..=b'z' => Ok((c - b'a') as u32 + 26),
        b'0'..=b'9' => Ok((c - b'0') as u32 + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(Error::Invalid(format!("bad base64 byte: {c:#x}"))),
    }
}

/// Decode standard base64; padding required, whitespace not tolerated.
pub fn decode(input: &str) -> Result<Vec<u8>> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(Error::Invalid(
            "base64 length must be a multiple of 4".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = (i + 1) * 4 == bytes.len();
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return Err(Error::Invalid("bad base64 padding".to_string()));
        }
        let vals = [
            value_of(chunk[0])?,
            value_of(chunk[1])?,
            if pad >= 2 { 0 } else { value_of(chunk[2])? },
            if pad >= 1 { 0 } else { value_of(chunk[3])? },
        ];
        let n = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for input in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"\x00\xff\x7f",
        ] {
            assert_eq!(decode(&encode(input)).unwrap(), input, "{input:?}");
        }
    }

    #[test]
    fn rfc4648_vectors() {
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(decode("Zm9vYmE=").unwrap(), b"fooba");
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("abc").is_err());
        assert!(decode("ab=c").is_err());
        assert!(decode("a bc").is_err());
    }
}
