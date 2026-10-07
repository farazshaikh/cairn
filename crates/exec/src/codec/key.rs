//! Order-preserving keys. Unsigned byte order of the encoded keys equals
//! the SQL order of the values they encode, with NULL first.
//!
//! **Row key** (table B-tree key), 8 bytes:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0 | 8 | `(k as u64) ^ 0x8000_0000_0000_0000`, big-endian |
//!
//! **Index key** (index B-tree key; the value is empty): the column value
//! encoded below, then the 8-byte row key of the row it points at.
//!
//! | Value | Encoding |
//! |-------|----------|
//! | NULL | `00` |
//! | INTEGER i | `01`, then the row-key encoding of i |
//! | REAL r | `01`, then 8 bytes big-endian: `b = r.to_bits()`, all bits flipped if the sign bit is set, otherwise only the sign bit flipped |
//! | BOOLEAN | `01`, then `00` (FALSE) or `01` (TRUE) |
//! | TEXT | `01`, then the UTF-8 bytes with each `00` written as `00 FF`, then the terminator `00 00` |
//!
//! Each column holds one type, so only values of one type share an index.
//! Example: TEXT `'a'` at row key 1 encodes as `01 61 00 00 80 00 00 00 00
//! 00 00 01`. Keys longer than `cairn_storage::MAX_KEY_LEN` (256 bytes)
//! cannot be stored.

use crate::value::{SqlType, Value};

const SIGN: u64 = 0x8000_0000_0000_0000;

pub fn row_key(key: i64) -> [u8; 8] {
    ((key as u64) ^ SIGN).to_be_bytes()
}

pub fn decode_row_key(bytes: &[u8]) -> Option<i64> {
    let bytes: [u8; 8] = bytes.try_into().ok()?;
    Some((u64::from_be_bytes(bytes) ^ SIGN) as i64)
}

/// The order-preserving encoding of one column value.
pub fn value_key(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match value {
        Value::Null => out.push(0),
        Value::Integer(v) => {
            out.push(1);
            out.extend_from_slice(&row_key(*v));
        }
        Value::Real(v) => {
            out.push(1);
            let bits = v.to_bits();
            let ordered = if bits & SIGN != 0 { !bits } else { bits ^ SIGN };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Boolean(v) => {
            out.push(1);
            out.push(u8::from(*v));
        }
        Value::Text(text) => {
            out.push(1);
            for &byte in text.as_bytes() {
                out.push(byte);
                if byte == 0 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0, 0]);
        }
    }
    out
}

pub fn index_key(value: &Value, key: i64) -> Vec<u8> {
    let mut out = value_key(value);
    out.extend_from_slice(&row_key(key));
    out
}

/// Splits an index key into the column value (of column type `ty`) and the
/// row key.
pub fn decode_index_key(bytes: &[u8], ty: SqlType) -> Result<(Value, i64), String> {
    let split = bytes.len().checked_sub(8).ok_or("index key is too short")?;
    let (value_bytes, key_bytes) = bytes.split_at(split);
    let key = decode_row_key(key_bytes).ok_or("index key is too short")?;
    let value = decode_value_key(value_bytes, ty)?;
    Ok((value, key))
}

fn decode_value_key(bytes: &[u8], ty: SqlType) -> Result<Value, String> {
    let Some((&marker, rest)) = bytes.split_first() else {
        return Err("empty index value".to_string());
    };
    match (marker, ty) {
        (0, _) if rest.is_empty() => Ok(Value::Null),
        (1, SqlType::Integer) => decode_row_key(rest)
            .map(Value::Integer)
            .ok_or_else(|| "bad integer in index key".to_string()),
        (1, SqlType::Real) => {
            let bytes: [u8; 8] = rest.try_into().map_err(|_| "bad real in index key")?;
            let ordered = u64::from_be_bytes(bytes);
            let bits = if ordered & SIGN != 0 {
                ordered ^ SIGN
            } else {
                !ordered
            };
            Ok(Value::Real(f64::from_bits(bits)))
        }
        (1, SqlType::Boolean) => match rest {
            [0] => Ok(Value::Boolean(false)),
            [1] => Ok(Value::Boolean(true)),
            _ => Err("bad boolean in index key".to_string()),
        },
        (1, SqlType::Text) => decode_text(rest),
        _ => Err("bad index value marker".to_string()),
    }
}

fn decode_text(bytes: &[u8]) -> Result<Value, String> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut iter = bytes.iter();
    while let Some(&byte) = iter.next() {
        if byte != 0 {
            out.push(byte);
            continue;
        }
        match iter.next() {
            Some(0xFF) => out.push(0),
            Some(0) if iter.as_slice().is_empty() => {
                return String::from_utf8(out)
                    .map(Value::Text)
                    .map_err(|_| "text in index key is not UTF-8".to_string());
            }
            _ => return Err("bad escape in index key text".to_string()),
        }
    }
    Err("unterminated text in index key".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::cmp_values;

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn generate(rng: &mut XorShift, ty: SqlType) -> Value {
        if rng.next().is_multiple_of(10) {
            return Value::Null;
        }
        match ty {
            SqlType::Integer => {
                let edges = [i64::MIN, -1, 0, 1, i64::MAX];
                match rng.next() % 4 {
                    0 => Value::Integer(edges[(rng.next() % 5) as usize]),
                    1 => Value::Integer((rng.next() % 200) as i64 - 100),
                    _ => Value::Integer(rng.next() as i64),
                }
            }
            SqlType::Real => loop {
                let candidate = match rng.next() % 3 {
                    0 => f64::from_bits(rng.next()),
                    1 => (rng.next() % 2000) as f64 / 8.0 - 125.0,
                    _ => [0.0, f64::MIN_POSITIVE, -5e-324, 5e-324, f64::MAX, f64::MIN]
                        [(rng.next() % 6) as usize],
                };
                if candidate.is_finite() {
                    break Value::real(candidate);
                }
            },
            SqlType::Boolean => Value::Boolean(rng.next().is_multiple_of(2)),
            SqlType::Text => {
                let alphabet = ['\0', 'a', 'b', 'z', 'é', '世', '\u{1}', '\u{ff}'];
                let len = (rng.next() % 6) as usize;
                Value::Text(
                    (0..len)
                        .map(|_| alphabet[(rng.next() % 8) as usize])
                        .collect(),
                )
            }
            SqlType::Null => Value::Null,
        }
    }

    #[test]
    fn byte_order_equals_value_order_and_keys_round_trip() {
        let types = [
            SqlType::Integer,
            SqlType::Real,
            SqlType::Boolean,
            SqlType::Text,
        ];
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for ty in types {
            let values: Vec<(Value, i64)> = (0..1000)
                .map(|_| (generate(&mut rng, ty), (rng.next() % 7) as i64 - 3))
                .collect();
            for (value, key) in &values {
                let encoded = index_key(value, *key);
                assert_eq!(decode_index_key(&encoded, ty), Ok((value.clone(), *key)));
            }
            let mut by_bytes = values.clone();
            by_bytes.sort_by_key(|(value, key)| index_key(value, *key));
            let mut by_value = values;
            by_value.sort_by(|(a, ka), (b, kb)| cmp_values(a, b).then(ka.cmp(kb)));
            let bytes_order: Vec<_> = by_bytes.iter().map(|(v, k)| index_key(v, *k)).collect();
            let value_order: Vec<_> = by_value.iter().map(|(v, k)| index_key(v, *k)).collect();
            assert_eq!(bytes_order, value_order, "{ty}");
        }
    }

    #[test]
    fn row_keys_order_like_integers() {
        let keys = [i64::MIN, -2, -1, 0, 1, 2, i64::MAX];
        for pair in keys.windows(2) {
            assert!(row_key(pair[0]) < row_key(pair[1]));
            assert_eq!(decode_row_key(&row_key(pair[0])), Some(pair[0]));
        }
    }

    #[test]
    fn documented_example_matches() {
        assert_eq!(
            index_key(&Value::Text("a".into()), 1),
            [1, b'a', 0, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn text_prefixes_and_nul_order_correctly() {
        let texts = ["", "\0", "\0\0", "a", "a\0", "a\0b", "ab", "b"];
        for pair in texts.windows(2) {
            assert!(
                value_key(&Value::Text(pair[0].into())) < value_key(&Value::Text(pair[1].into())),
                "{pair:?}"
            );
        }
    }

    #[test]
    fn malformed_index_keys_are_errors() {
        assert!(decode_index_key(&[1, 2, 3], SqlType::Integer).is_err());
        let mut text = index_key(&Value::Text("a".into()), 1);
        text[2] = 7;
        assert!(decode_index_key(&text, SqlType::Text).is_err());
    }
}
