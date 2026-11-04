//! Values, types and errors.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::keycode;

/// An error with a PostgreSQL SQLSTATE code, so the wire server can pass it on.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Error {
        Error { code, message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl From<crate::mvcc::Error> for Error {
    fn from(e: crate::mvcc::Error) -> Self {
        match e {
            crate::mvcc::Error::Serialization => Error::new("40001", e.to_string()),
            crate::mvcc::Error::ReadOnly => Error::new("25006", e.to_string()),
            crate::mvcc::Error::Io(io) => match io.kind() {
                std::io::ErrorKind::InvalidInput => Error::new("54000", format!("row too large: {io}")),
                _ => Error::new("58030", format!("I/O error: {io}")),
            },
        }
    }
}

macro_rules! err {
    ($code:expr, $($arg:tt)*) => { Err($crate::sql::types::Error::new($code, format!($($arg)*))) };
}
pub(crate) use err;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DataType {
    Bool,
    Int,
    Float,
    Text,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            DataType::Bool => "BOOLEAN",
            DataType::Int => "INTEGER",
            DataType::Float => "FLOAT",
            DataType::Text => "TEXT",
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

pub type Row = Vec<Value>;

impl Value {
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(DataType::Bool),
            Value::Int(_) => Some(DataType::Int),
            Value::Float(_) => Some(DataType::Float),
            Value::Text(_) => Some(DataType::Text),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// SQL truth: NULL is unknown (None). Numbers count as true when non-zero (as in SQLite).
    pub fn truth(&self) -> Result<Option<bool>> {
        match self {
            Value::Null => Ok(None),
            Value::Bool(b) => Ok(Some(*b)),
            Value::Int(i) => Ok(Some(*i != 0)),
            Value::Float(f) => Ok(Some(*f != 0.0)),
            Value::Text(s) => match s.to_ascii_lowercase().as_str() {
                "t" | "true" | "1" | "yes" | "on" => Ok(Some(true)),
                "f" | "false" | "0" | "no" | "off" => Ok(Some(false)),
                _ => err!("22P02", "invalid input syntax for type boolean: \"{s}\""),
            },
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// SQL comparison: None when either side is NULL. A string compared with a number is read
    /// as a number (literals like '5' are untyped in SQL).
    pub fn compare(&self, other: &Value) -> Result<Option<Ordering>> {
        use Value::*;
        Ok(Some(match (self, other) {
            (Null, _) | (_, Null) => return Ok(None),
            (Int(a), Int(b)) => a.cmp(b),
            (Int(_) | Float(_), Int(_) | Float(_)) => {
                let (a, b) = (self.as_f64().unwrap(), other.as_f64().unwrap());
                a.partial_cmp(&b).unwrap_or_else(|| a.total_cmp(&b))
            }
            (Text(a), Text(b)) => a.cmp(b),
            (Bool(a), Bool(b)) => a.cmp(b),
            (Text(s), n @ (Int(_) | Float(_))) => return Value::parse_number(s)?.compare(n),
            (n @ (Int(_) | Float(_)), Text(s)) => return n.compare(&Value::parse_number(s)?),
            (Bool(_), _) | (_, Bool(_)) => {
                let (a, b) = (self.cast(DataType::Bool)?, other.cast(DataType::Bool)?);
                return a.compare(&b);
            }
        }))
    }

    fn parse_number(s: &str) -> Result<Value> {
        let t = s.trim();
        if let Ok(i) = t.parse::<i64>() {
            return Ok(Value::Int(i));
        }
        match t.parse::<f64>() {
            Ok(f) => Ok(Value::Float(f)),
            Err(_) => err!("22P02", "invalid input syntax for a number: \"{s}\""),
        }
    }

    /// A total order for sorting and grouping: NULL first, then by type, then by value.
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        use Value::*;
        let rank = |v: &Value| match v {
            Null => 0,
            Bool(_) => 1,
            Int(_) | Float(_) => 2,
            Text(_) => 3,
        };
        match (self, other) {
            (Int(a), Int(b)) => a.cmp(b),
            (Int(_) | Float(_), Int(_) | Float(_)) => {
                self.as_f64().unwrap().total_cmp(&other.as_f64().unwrap())
            }
            (Text(a), Text(b)) => a.cmp(b),
            (Bool(a), Bool(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }

    pub fn cast(&self, to: DataType) -> Result<Value> {
        use Value::*;
        Ok(match (self, to) {
            (Null, _) => Null,
            (Bool(b), DataType::Bool) => Bool(*b),
            (Bool(b), DataType::Int) => Int(*b as i64),
            (Bool(b), DataType::Float) => Float(*b as i64 as f64),
            (Bool(b), DataType::Text) => Text(if *b { "true" } else { "false" }.into()),
            (Int(i), DataType::Int) => Int(*i),
            (Int(i), DataType::Float) => Float(*i as f64),
            (Int(i), DataType::Bool) => Bool(*i != 0),
            (Int(i), DataType::Text) => Text(i.to_string()),
            (Float(f), DataType::Float) => Float(*f),
            (Float(f), DataType::Int) => {
                if f.is_nan() || f.abs() >= 9.223372036854775807e18 {
                    return err!("22003", "integer out of range");
                }
                Int(f.round() as i64)
            }
            (Float(f), DataType::Bool) => Bool(*f != 0.0),
            (Float(f), DataType::Text) => Text(format_float(*f)),
            (Text(s), DataType::Text) => Text(s.clone()),
            (Text(s), DataType::Int) => match s.trim().parse::<i64>() {
                Ok(i) => Int(i),
                Err(_) => match s.trim().parse::<f64>() {
                    Ok(f) if f.fract() == 0.0 => Int(f as i64),
                    _ => return err!("22P02", "invalid input syntax for type integer: \"{s}\""),
                },
            },
            (Text(s), DataType::Float) => match s.trim().parse::<f64>() {
                Ok(f) => Float(f),
                Err(_) => return err!("22P02", "invalid input syntax for type float: \"{s}\""),
            },
            (Text(_), DataType::Bool) => Bool(self.truth()?.unwrap()),
        })
    }

    /// Order-preserving key bytes (for primary keys and indexes).
    pub fn encode_key(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.push(0x01),
            Value::Bool(false) => out.push(0x02),
            Value::Bool(true) => out.push(0x03),
            Value::Int(i) => {
                out.push(0x04);
                keycode::put_i64(out, *i);
            }
            Value::Float(f) => {
                out.push(0x05);
                keycode::put_f64(out, *f);
            }
            Value::Text(s) => {
                out.push(0x06);
                keycode::put_bytes(out, s.as_bytes());
            }
        }
    }

    pub fn decode_key(input: &[u8]) -> Option<(Value, &[u8])> {
        let (&tag, rest) = input.split_first()?;
        Some(match tag {
            0x01 => (Value::Null, rest),
            0x02 => (Value::Bool(false), rest),
            0x03 => (Value::Bool(true), rest),
            0x04 => keycode::take_i64(rest).map(|(i, r)| (Value::Int(i), r))?,
            0x05 => keycode::take_f64(rest).map(|(f, r)| (Value::Float(f), r))?,
            0x06 => keycode::take_bytes(rest).map(|(b, r)| (Value::Text(String::from_utf8_lossy(&b).into_owned()), r))?,
            _ => return None,
        })
    }
}

pub fn format_float(f: f64) -> String {
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f.is_nan() {
        return "NaN".into();
    }
    let s = format!("{f}");
    if f.fract() == 0.0 && f.abs() < 1e16 && !s.contains('e') { s } else { s }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(b) => f.write_str(if *b { "TRUE" } else { "FALSE" }),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => f.write_str(&format_float(*x)),
            Value::Text(s) => write!(f, "'{}'", s.replace('\'', "''")),
        }
    }
}

/// Equality for grouping, DISTINCT and hash joins: NULL equals NULL, 1 equals 1.0.
impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        self.sort_cmp(other) == Ordering::Equal && (self.is_null() == other.is_null())
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self {
            Value::Null => 0u8.hash(h),
            Value::Bool(b) => b.hash(h),
            Value::Int(i) => (*i as f64).to_bits().hash(h),
            Value::Float(f) => (if *f == 0.0 { 0.0f64 } else { *f }).to_bits().hash(h),
            Value::Text(s) => s.hash(h),
        }
    }
}

/// Compact row encoding for table storage.
pub fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(row.len() * 9);
    for v in row {
        match v {
            Value::Null => out.push(0),
            Value::Bool(false) => out.push(1),
            Value::Bool(true) => out.push(2),
            Value::Int(i) => {
                out.push(3);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Float(f) => {
                out.push(4);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Text(s) => {
                out.push(5);
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    out
}

pub fn decode_row(mut b: &[u8]) -> Result<Row> {
    let bad = || Error::new("XX001", "corrupt row");
    let mut row = Vec::new();
    while let Some((&tag, rest)) = b.split_first() {
        b = rest;
        let take = |b: &mut &[u8], n: usize| -> Result<Vec<u8>> {
            if b.len() < n {
                return Err(bad());
            }
            let (x, r) = b.split_at(n);
            *b = r;
            Ok(x.to_vec())
        };
        row.push(match tag {
            0 => Value::Null,
            1 => Value::Bool(false),
            2 => Value::Bool(true),
            3 => Value::Int(i64::from_le_bytes(take(&mut b, 8)?.try_into().unwrap())),
            4 => Value::Float(f64::from_le_bytes(take(&mut b, 8)?.try_into().unwrap())),
            5 => {
                let n = u32::from_le_bytes(take(&mut b, 4)?.try_into().unwrap()) as usize;
                Value::Text(String::from_utf8(take(&mut b, n)?).map_err(|_| bad())?)
            }
            _ => return Err(bad()),
        });
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons_and_casts() {
        assert_eq!(Value::Int(1).compare(&Value::Float(1.5)).unwrap(), Some(Ordering::Less));
        assert_eq!(Value::Null.compare(&Value::Int(1)).unwrap(), None);
        assert_eq!(Value::Text("10".into()).compare(&Value::Int(9)).unwrap(), Some(Ordering::Greater));
        assert!(Value::Text("x".into()).compare(&Value::Int(9)).is_err());
        assert_eq!(Value::Float(2.5).cast(DataType::Int).unwrap(), Value::Int(3));
        assert_eq!(Value::Text(" 42 ".into()).cast(DataType::Int).unwrap(), Value::Int(42));
        assert_eq!(Value::Int(1), Value::Float(1.0));
        assert_eq!(Value::Null, Value::Null);
    }

    #[test]
    fn rows_and_keys_round_trip() {
        let row = vec![Value::Null, Value::Bool(true), Value::Int(-7), Value::Float(2.5), Value::Text("héllo".into())];
        assert_eq!(decode_row(&encode_row(&row)).unwrap(), row);
        let mut a = Vec::new();
        Value::Int(-3).encode_key(&mut a);
        let mut b = Vec::new();
        Value::Int(2).encode_key(&mut b);
        assert!(a < b);
        let mut k = Vec::new();
        for v in &row {
            v.encode_key(&mut k);
        }
        let mut rest = k.as_slice();
        for v in &row {
            let (d, r) = Value::decode_key(rest).unwrap();
            assert_eq!(&d, v);
            rest = r;
        }
    }
}
