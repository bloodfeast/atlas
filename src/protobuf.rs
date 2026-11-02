//! A Protocol Buffers wire-format reader: just enough to walk a message
//! field by field over a byte slice, with no schema and no allocation.
//!
//! The wire format (protobuf.dev, "Encoding"): a message is a run of
//! fields, each a *tag* varint `(field_number << 3) | wire_type` followed by
//! its payload:
//!
//! | wire type | payload |
//! |---|---|
//! | 0 `VARINT` | a base-128 varint (int32, int64, uint, bool, enum) |
//! | 1 `I64` | 8 bytes little-endian |
//! | 2 `LEN` | a varint length, then that many bytes (string, bytes, a nested message, a packed repeated field) |
//! | 5 `I32` | 4 bytes little-endian |
//!
//! Wire types 3 and 4 (groups) are deprecated and refused. Every read is
//! bounds-checked; a truncated or malformed buffer is an error, never a
//! panic. Unknown fields are skipped by the caller with [`Reader::skip`],
//! as protobuf requires.

use std::fmt;

/// Why a buffer could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    /// What was wrong.
    pub what: &'static str,
    /// The byte offset in the buffer being read.
    pub at: usize,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "protobuf: {} at byte {}", self.what, self.at)
    }
}

impl std::error::Error for DecodeError {}

/// The payload shape of a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    /// A varint.
    Varint,
    /// Eight bytes.
    I64,
    /// A length-prefixed run of bytes.
    Len,
    /// Four bytes.
    I32,
}

/// A pull reader over one message's bytes.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Starts reading a message.
    ///
    /// # Arguments
    ///
    /// * `buf` - the message bytes (a whole file, or a `LEN` payload).
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    /// The current offset into the buffer.
    pub fn pos(&self) -> usize {
        self.pos
    }

    fn err(&self, what: &'static str) -> DecodeError {
        DecodeError { what, at: self.pos }
    }

    /// Reads the next field's tag.
    ///
    /// # Returns
    ///
    /// `None` at the end of the message, else the field number and its
    /// wire type.
    ///
    /// # Errors
    ///
    /// A bad varint, field number 0, or a group or unknown wire type.
    pub fn field(&mut self) -> Result<Option<(u32, WireType)>, DecodeError> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let tag = self.varint()?;
        let number = tag >> 3;
        if number == 0 || number > u64::from(u32::MAX >> 3) {
            return Err(self.err("bad field number"));
        }
        let wt = match tag & 7 {
            0 => WireType::Varint,
            1 => WireType::I64,
            2 => WireType::Len,
            5 => WireType::I32,
            _ => return Err(self.err("unsupported wire type")),
        };
        Ok(Some((number as u32, wt)))
    }

    /// Reads a base-128 varint (at most 10 bytes).
    ///
    /// # Errors
    ///
    /// A varint running past the buffer or over 10 bytes.
    #[inline]
    pub fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        let mut shift = 0u32;
        loop {
            let Some(&b) = self.buf.get(self.pos) else {
                return Err(self.err("truncated varint"));
            };
            self.pos += 1;
            if shift == 63 && b > 1 {
                return Err(self.err("varint overflows 64 bits"));
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(self.err("varint over 10 bytes"));
            }
        }
    }

    /// Reads a varint as an `int32` (protobuf sign-extends negative int32
    /// values to 64 bits, so the low 32 bits are the value).
    ///
    /// # Errors
    ///
    /// As [`Reader::varint`].
    #[inline]
    pub fn int32(&mut self) -> Result<i32, DecodeError> {
        Ok(self.varint()? as i32)
    }

    /// Reads a `LEN` payload.
    ///
    /// # Errors
    ///
    /// A bad length varint or a length running past the buffer.
    #[inline]
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.varint()?;
        let end = usize::try_from(n).ok().and_then(|n| self.pos.checked_add(n)).filter(|&e| e <= self.buf.len());
        let Some(end) = end else {
            return Err(self.err("length runs past the buffer"));
        };
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// Reads a `LEN` payload as UTF-8 text.
    ///
    /// # Errors
    ///
    /// As [`Reader::bytes`], or the bytes are not UTF-8.
    pub fn string(&mut self) -> Result<&'a str, DecodeError> {
        let at = self.pos;
        let b = self.bytes()?;
        std::str::from_utf8(b).map_err(|_| DecodeError { what: "string is not UTF-8", at })
    }

    /// Reads a repeated `int32` field occurrence: packed (`LEN`) or a single
    /// varint, both of which a reader must accept. Values are appended to
    /// `out`.
    ///
    /// # Errors
    ///
    /// As [`Reader::bytes`] and [`Reader::varint`], or a wire type that is
    /// neither.
    pub fn int32s(&mut self, wt: WireType, out: &mut Vec<i32>) -> Result<(), DecodeError> {
        match wt {
            WireType::Varint => out.push(self.int32()?),
            WireType::Len => {
                let mut r = Reader::new(self.bytes()?);
                while r.pos < r.buf.len() {
                    out.push(r.int32()?);
                }
            }
            _ => return Err(self.err("repeated int32 with a fixed-width wire type")),
        }
        Ok(())
    }

    /// Skips a field's payload.
    ///
    /// # Errors
    ///
    /// A payload running past the buffer.
    pub fn skip(&mut self, wt: WireType) -> Result<(), DecodeError> {
        let n = match wt {
            WireType::Varint => {
                self.varint()?;
                return Ok(());
            }
            WireType::Len => {
                self.bytes()?;
                return Ok(());
            }
            WireType::I64 => 8,
            WireType::I32 => 4,
        };
        if self.pos + n > self.buf.len() {
            return Err(self.err("fixed field runs past the buffer"));
        }
        self.pos += n;
        Ok(())
    }
}

/// A protobuf writer, for building test messages (the index itself is
/// only ever read).
#[derive(Debug, Default, Clone)]
pub struct Writer {
    /// The bytes written so far.
    pub buf: Vec<u8>,
}

impl Writer {
    /// Appends a varint.
    pub fn varint(&mut self, mut v: u64) -> &mut Self {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
        self
    }

    fn tag(&mut self, field: u32, wt: u8) -> &mut Self {
        self.varint((u64::from(field) << 3) | u64::from(wt))
    }

    /// Appends a varint field (an `int32` is sign-extended as protobuf does).
    pub fn int(&mut self, field: u32, v: i64) -> &mut Self {
        self.tag(field, 0).varint(v as u64)
    }

    /// Appends a `LEN` field.
    pub fn bytes(&mut self, field: u32, b: &[u8]) -> &mut Self {
        self.tag(field, 2).varint(b.len() as u64);
        self.buf.extend_from_slice(b);
        self
    }

    /// Appends a string field.
    pub fn string(&mut self, field: u32, s: &str) -> &mut Self {
        self.bytes(field, s.as_bytes())
    }

    /// Appends a nested message field.
    pub fn message(&mut self, field: u32, m: &Writer) -> &mut Self {
        self.bytes(field, &m.buf)
    }

    /// Appends a packed repeated `int32` field.
    pub fn packed(&mut self, field: u32, vals: &[i32]) -> &mut Self {
        let mut w = Writer::default();
        for &v in vals {
            w.varint(i64::from(v) as u64);
        }
        self.bytes(field, &w.buf)
    }

    /// Appends a fixed 32-bit field.
    pub fn fixed32(&mut self, field: u32, v: u32) -> &mut Self {
        self.tag(field, 5);
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Appends a fixed 64-bit field.
    pub fn fixed64(&mut self, field: u32, v: u64) -> &mut Self {
        self.tag(field, 1);
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_at_every_width() {
        for v in [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX >> 1, u64::MAX] {
            let mut w = Writer::default();
            w.varint(v);
            let mut r = Reader::new(&w.buf);
            assert_eq!(r.varint().unwrap(), v);
            assert_eq!(r.pos(), w.buf.len());
        }
        // The encoding example from the protobuf docs: 150 is 96 01.
        assert_eq!(Reader::new(&[0x96, 0x01]).varint().unwrap(), 150);
    }

    #[test]
    fn negative_int32_is_ten_bytes_and_reads_back() {
        let mut w = Writer::default();
        w.int(1, -5);
        assert_eq!(w.buf.len(), 11);
        let mut r = Reader::new(&w.buf);
        assert_eq!(r.field().unwrap(), Some((1, WireType::Varint)));
        assert_eq!(r.int32().unwrap(), -5);
    }

    #[test]
    fn fields_packed_and_unpacked_and_skips() {
        let mut inner = Writer::default();
        inner.string(1, "hi");
        let mut w = Writer::default();
        w.packed(1, &[3, 0, 9]).int(1, 7).fixed32(9, 5).fixed64(10, 6).message(2, &inner).string(3, "é");
        let mut r = Reader::new(&w.buf);
        let mut ints = Vec::new();
        let mut seen = Vec::new();
        while let Some((f, wt)) = r.field().unwrap() {
            seen.push(f);
            match f {
                1 => r.int32s(wt, &mut ints).unwrap(),
                2 => {
                    let mut m = Reader::new(r.bytes().unwrap());
                    assert_eq!(m.field().unwrap(), Some((1, WireType::Len)));
                    assert_eq!(m.string().unwrap(), "hi");
                    assert_eq!(m.field().unwrap(), None);
                }
                3 => assert_eq!(r.string().unwrap(), "é"),
                _ => r.skip(wt).unwrap(),
            }
        }
        assert_eq!(ints, [3, 0, 9, 7]);
        assert_eq!(seen, [1, 1, 9, 10, 2, 3]);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        let cases: &[&[u8]] = &[
            &[0x80],                   // truncated varint
            &[0x0a, 0x05, b'a'],       // length past the end
            &[0x0b],                   // group start
            &[0x00],                   // field 0
            &[0x0d, 1, 2],             // fixed32 past the end
            &[0xff; 11],               // varint over 10 bytes
            &[0x12, 0x02, 0xff, 0xfe], // string not UTF-8
        ];
        for c in cases {
            let mut r = Reader::new(c);
            let mut failed = false;
            for _ in 0..4 {
                match r.field() {
                    Err(_) => {
                        failed = true;
                        break;
                    }
                    Ok(None) => break,
                    Ok(Some((_, wt))) => {
                        let res = if wt == WireType::Len { r.string().map(|_| ()) } else { r.skip(wt) };
                        if res.is_err() {
                            failed = true;
                            break;
                        }
                    }
                }
            }
            assert!(failed, "accepted {c:?}");
        }
    }
}
