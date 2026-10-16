//! Versioned Windows stop record. Capabilities are never formatted for diagnostics.
//! Process liveness and authority to stop/delete are deliberately outside this codec.

use std::io;

pub(super) const MAX_BYTES: usize = 256;
const VERSION: &[u8] = b"gentle-dot-stop-v1";

// Not Copy or Debug: callers must explicitly retain ownership and avoid secret logging.
#[derive(PartialEq, Eq)]
pub(crate) struct Record {
    pub(super) pid: u32,
    pub(super) created: u64,
    nonce: [u8; 16],
    capability: [u8; 32],
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid Windows stop record")
}

fn decimal(bytes: &[u8]) -> io::Result<u64> {
    if bytes.is_empty() || bytes.len() > 20 || (bytes.len() > 1 && bytes[0] == b'0') {
        return Err(invalid());
    }
    bytes.iter().try_fold(0u64, |value, byte| {
        if !byte.is_ascii_digit() { return Err(invalid()); }
        value.checked_mul(10).and_then(|n| n.checked_add((byte - b'0') as u64))
            .ok_or_else(invalid)
    })
}

fn unhex<const N: usize>(bytes: &[u8]) -> io::Result<[u8; N]> {
    if bytes.len() != N * 2 { return Err(invalid()); }
    let digit = |byte| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(invalid()),
    };
    let mut result = [0; N];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        result[index] = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Ok(result)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

impl Record {
    pub(super) fn new(pid: u32, created: u64, nonce: [u8; 16], capability: [u8; 32]) -> io::Result<Self> {
        if pid <= 1 || created == 0 { return Err(invalid()); }
        Ok(Self { pid, created, nonce, capability })
    }

    pub(super) fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_BYTES || bytes.last() != Some(&b'\n')
            || !bytes.iter().all(|byte| *byte == b'\n' || (32..=126).contains(byte)) {
            return Err(invalid());
        }
        let mut fields = bytes[..bytes.len() - 1].split(|byte| *byte == b'\n');
        if fields.next() != Some(VERSION) { return Err(invalid()); }
        let pid = u32::try_from(decimal(fields.next().ok_or_else(invalid)?)?).map_err(|_| invalid())?;
        let created = decimal(fields.next().ok_or_else(invalid)?)?;
        let nonce = unhex(fields.next().ok_or_else(invalid)?)?;
        let capability = unhex(fields.next().ok_or_else(invalid)?)?;
        if fields.next().is_some() { return Err(invalid()); }
        Self::new(pid, created, nonce, capability)
    }

    pub(super) fn encode(&self) -> Vec<u8> {
        format!("gentle-dot-stop-v1\n{}\n{}\n{}\n{}\n", self.pid, self.created,
            hex(&self.nonce), hex(&self.capability)).into_bytes()
    }

    pub(super) fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\gentle-dot-stop-{}", hex(&self.nonce))
    }

    pub(super) fn frame(&self) -> [u8; 36] {
        let mut frame = [0; 36];
        frame[..4].copy_from_slice(b"GDS1");
        frame[4..].copy_from_slice(&self.capability);
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record { Record::new(42, 123, [0xab; 16], [0xcd; 32]).unwrap() }
    fn text(pid: &str, created: &str, nonce: &str, capability: &str) -> Vec<u8> {
        format!("gentle-dot-stop-v1\n{pid}\n{created}\n{nonce}\n{capability}\n").into_bytes()
    }

    #[test]
    fn roundtrip_max_decimal_identities_pipe_and_exact_frame() {
        let record = Record::new(u32::MAX, u64::MAX, [0xab; 16], [0xcd; 32]).unwrap();
        let encoded = record.encode();
        assert!(encoded.len() <= MAX_BYTES);
        assert!(Record::parse(&encoded).unwrap() == record);
        assert_eq!(record.pipe_name(), format!(r"\\.\pipe\gentle-dot-stop-{}", "ab".repeat(16)));
        let frame = record.frame();
        assert_eq!(frame.len(), 36);
        assert_eq!(&frame[..4], b"GDS1");
        assert_eq!(&frame[4..], &[0xcd; 32]);
    }

    #[test]
    fn exact_version_field_count_final_newline_and_no_trailing_bytes() {
        let good = record().encode();
        assert!(Record::parse(&good).is_ok());
        assert!(Record::parse(&good[..good.len() - 1]).is_err());
        for suffix in [b"\n".as_slice(), b"x", b"\0", b"extra\n"] {
            let mut bytes = good.clone(); bytes.extend_from_slice(suffix);
            assert!(Record::parse(&bytes).is_err());
        }
        let mut version = good.clone(); version[17] = b'2';
        assert!(Record::parse(&version).is_err());
        assert!(Record::parse(b"gentle-dot-stop-v1\n42\n123\n").is_err());
        assert!(Record::parse(b"42 123\n").is_err());
        let crlf: Vec<_> = good.iter().flat_map(|byte| {
            if *byte == b'\n' { vec![b'\r', b'\n'] } else { vec![*byte] }
        }).collect();
        assert!(Record::parse(&crlf).is_err());
    }

    #[test]
    fn bounded_ascii_and_decimal_canonicality() {
        let nonce = "ab".repeat(16); let capability = "cd".repeat(32);
        for pid in ["", "0", "1", "01", "042", "-42", "+42", "42 ", "4294967296", "18446744073709551616"] {
            assert!(Record::parse(&text(pid, "123", &nonce, &capability)).is_err());
        }
        for created in ["", "0", "00", "0123", "-123", "+123", "123 ", "18446744073709551616"] {
            assert!(Record::parse(&text("42", created, &nonce, &capability)).is_err());
        }
        let mut bytes = record().encode(); bytes[0] = 0xff;
        assert!(Record::parse(&bytes).is_err());
        for len in [256, 257] { assert!(Record::parse(&vec![b'\n'; len]).is_err()); }
        assert!(Record::parse(&[]).is_err());
    }

    #[test]
    fn fixed_lowercase_hex_only() {
        let nonce = "ab".repeat(16); let capability = "cd".repeat(32);
        for bad in ["a".repeat(31), "a".repeat(33), "AB".repeat(16), "gg".repeat(16)] {
            assert!(Record::parse(&text("42", "123", &bad, &capability)).is_err());
        }
        for bad in ["c".repeat(63), "c".repeat(65), "CD".repeat(32), "zz".repeat(32)] {
            assert!(Record::parse(&text("42", "123", &nonce, &bad)).is_err());
        }
    }

}
