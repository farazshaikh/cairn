//! Row record encoding, version 1: the B-tree value of a table row.
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0 | 1 | version = 1 |
//! | 1 | 2 | column count (u16 LE); must equal the catalog column count |
//! | 3 | var | one tagged value per column, in declaration order |
//!
//! Each value is a tag byte followed by its payload:
//!
//! | Tag | Type | Payload |
//! |-----|------|---------|
//! | 0 | NULL | none |
//! | 1 | INTEGER | 8 bytes, i64 LE |
//! | 2 | REAL | 8 bytes, `f64::to_bits` LE; finite and never `-0.0` |
//! | 3 | TEXT | u16 LE byte length, then UTF-8 |
//! | 4 | BOOLEAN | 1 byte, 0 or 1 |
//!
//! Example: `(7, NULL, 'hi')` encodes as `01 0300 01 0700000000000000 00
//! 03 0200 6869`. A record longer than `cairn_storage::MAX_VALUE_LEN`
//! (1024 bytes) cannot be stored. Decoding never panics: unknown versions
//! and tags, truncation, trailing bytes, invalid UTF-8, boolean bytes other
//! than 0 or 1 and non-finite reals are errors.

use crate::value::Value;

pub const RECORD_VERSION: u8 = 1;

const TAG_NULL: u8 = 0;
const TAG_INTEGER: u8 = 1;
const TAG_REAL: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BOOLEAN: u8 = 4;

/// Encodes a row. Callers check the encoded length against the storage
/// limit, which also bounds every text well below 65535 bytes.
pub fn encode_record(values: &[Value]) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    let count = u16::try_from(values.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for value in values {
        encode_value(&mut out, value);
    }
    out
}

fn encode_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Integer(v) => {
            out.push(TAG_INTEGER);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Real(v) => {
            out.push(TAG_REAL);
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        Value::Text(text) => {
            out.push(TAG_TEXT);
            match u16::try_from(text.len()) {
                Ok(len) => {
                    out.extend_from_slice(&len.to_le_bytes());
                    out.extend_from_slice(text.as_bytes());
                }
                // Too long for the format; pad so the size check rejects it.
                Err(_) => out.resize(out.len() + text.len() + 2, 0),
            }
        }
        Value::Boolean(v) => {
            out.push(TAG_BOOLEAN);
            out.push(u8::from(*v));
        }
    }
}

/// Decodes a record that must hold exactly `columns` values. The error is
/// a short reason for a `corrupt record` message.
pub fn decode_record(bytes: &[u8], columns: usize) -> Result<Vec<Value>, String> {
    let mut reader = Reader { bytes, pos: 0 };
    let version = reader.u8()?;
    if version != RECORD_VERSION {
        return Err(format!("unknown record version {version}"));
    }
    let count = usize::from(reader.u16()?);
    if count != columns {
        return Err(format!("record has {count} columns, expected {columns}"));
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(reader.value()?);
    }
    if reader.pos != bytes.len() {
        return Err("trailing bytes after the last column".to_string());
    }
    Ok(values)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Result<&[u8], String> {
        let end = self.pos.checked_add(len).ok_or("length overflow")?;
        let slice = self.bytes.get(self.pos..end).ok_or("record is truncated")?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?.first().copied().unwrap_or(0))
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| "record is truncated")?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| "record is truncated")?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.u8()? {
            TAG_NULL => Ok(Value::Null),
            TAG_INTEGER => Ok(Value::Integer(self.u64()? as i64)),
            TAG_REAL => {
                let value = f64::from_bits(self.u64()?);
                if !value.is_finite() || value.to_bits() == (-0.0f64).to_bits() {
                    return Err("invalid real value".to_string());
                }
                Ok(Value::Real(value))
            }
            TAG_TEXT => {
                let len = usize::from(self.u16()?);
                let bytes = self.take(len)?;
                let text = std::str::from_utf8(bytes).map_err(|_| "text is not UTF-8")?;
                Ok(Value::Text(text.to_string()))
            }
            TAG_BOOLEAN => match self.u8()? {
                0 => Ok(Value::Boolean(false)),
                1 => Ok(Value::Boolean(true)),
                other => Err(format!("invalid boolean byte {other}")),
            },
            other => Err(format!("unknown value tag {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Integer(0),
            Value::Real(1.5),
            Value::real(-0.0),
            Value::Real(f64::MAX),
            Value::Real(-5e-324),
            Value::Text(String::new()),
            Value::Text("héllo, 世界 🌍".to_string()),
            Value::Boolean(true),
            Value::Boolean(false),
        ]
    }

    #[test]
    fn every_value_round_trips() {
        let values = sample();
        let bytes = encode_record(&values);
        assert_eq!(decode_record(&bytes, values.len()), Ok(values));
    }

    #[test]
    fn documented_example_matches() {
        let values = vec![Value::Integer(7), Value::Null, Value::Text("hi".into())];
        let expected = [1, 3, 0, 1, 7, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 0, b'h', b'i'];
        assert_eq!(encode_record(&values), expected);
    }

    #[test]
    fn every_truncation_is_an_error() {
        let values = sample();
        let bytes = encode_record(&values);
        for len in 0..bytes.len() {
            assert!(
                decode_record(&bytes[..len], values.len()).is_err(),
                "prefix {len}"
            );
        }
    }

    #[test]
    fn malformed_records_are_errors() {
        let good = encode_record(&[Value::Boolean(true)]);
        let mut version = good.clone();
        version[0] = 2;
        assert_eq!(
            decode_record(&version, 1),
            Err("unknown record version 2".into())
        );
        let mut tag = good.clone();
        tag[3] = 9;
        assert_eq!(decode_record(&tag, 1), Err("unknown value tag 9".into()));
        let mut boolean = good.clone();
        boolean[4] = 2;
        assert!(decode_record(&boolean, 1).is_err());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_record(&trailing, 1).is_err());
        assert!(decode_record(&good, 2).is_err());
        let nan = encode_record(&[Value::Real(f64::NAN)]);
        assert!(decode_record(&nan, 1).is_err());
        let negative_zero = encode_record(&[Value::Real(-0.0)]);
        assert!(decode_record(&negative_zero, 1).is_err());
        let bad_utf8 = [1, 1, 0, TAG_TEXT, 1, 0, 0xFF];
        assert!(decode_record(&bad_utf8, 1).is_err());
    }

    #[test]
    fn oversize_text_cannot_slip_under_the_limit() {
        let text = "x".repeat(70_000);
        let bytes = encode_record(&[Value::Text(text)]);
        assert!(bytes.len() > cairn_storage::MAX_VALUE_LEN);
    }
}
