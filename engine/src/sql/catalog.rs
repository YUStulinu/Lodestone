//! Tables, indexes and views, stored as versioned keys like the data, so DDL is transactional.
//!
//! ```text
//! 0x10 name                     table definition (JSON)
//! 0x11 table_id key             a row; key = encoded primary key, or the hidden row id
//! 0x12 index_id values [key]    an index entry -> the row's key; unique indexes leave the row
//!                               key out of the entry's key (so two writers of one value conflict)
//! 0x13 name                     view definition (SQL)
//! ```

use serde::{Deserialize, Serialize};

use super::types::{DataType, Error, Result, Row, Value, err};
use crate::mvcc::{Range, Transaction};

pub const TABLE: u8 = 0x10;
pub const ROW: u8 = 0x11;
pub const INDEX: u8 = 0x12;
pub const VIEW: u8 = 0x13;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub default: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Index {
    pub id: u32,
    pub name: String,
    pub columns: Vec<usize>,
    pub unique: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Table {
    pub id: u32,
    pub name: String,
    pub columns: Vec<Column>,
    /// Primary key columns; empty means rows are keyed by a hidden row id.
    pub primary_key: Vec<usize>,
    pub indexes: Vec<Index>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct View {
    pub name: String,
    pub sql: String,
    pub columns: Vec<String>,
}

fn named(kind: u8, name: &str) -> Vec<u8> {
    let mut k = vec![kind];
    k.extend_from_slice(name.as_bytes());
    k
}

/// Definitions can outgrow one B+tree entry, so they are stored in chunks:
/// `key 0x00 chunk_no(u16)`. Names never contain 0x00, so one name's chunks are contiguous.
const CHUNK: usize = 700;

fn chunk_key(key: &[u8], i: u16) -> Vec<u8> {
    let mut k = key.to_vec();
    k.push(0);
    k.extend_from_slice(&i.to_be_bytes());
    k
}

fn get_blob(txn: &Transaction, key: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut prefix = key.to_vec();
    prefix.push(0);
    let range = Range { prefix: &prefix, from: std::ops::Bound::Unbounded, to: std::ops::Bound::Unbounded };
    let parts = txn.scan(&range, usize::MAX)?;
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts.into_iter().flat_map(|(_, v)| v).collect()))
}

fn put_blob(txn: &Transaction, key: &[u8], data: &[u8]) -> Result<()> {
    let old = get_blob(txn, key)?.map_or(0, |b| b.len().div_ceil(CHUNK).max(1));
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[]] } else { data.chunks(CHUNK).collect() };
    for (i, c) in chunks.iter().enumerate() {
        txn.set(&chunk_key(key, i as u16), c)?;
    }
    for i in chunks.len()..old {
        txn.delete(&chunk_key(key, i as u16))?;
    }
    Ok(())
}

fn delete_blob(txn: &Transaction, key: &[u8]) -> Result<()> {
    let n = get_blob(txn, key)?.map_or(0, |b| b.len().div_ceil(CHUNK).max(1));
    for i in 0..n {
        txn.delete(&chunk_key(key, i as u16))?;
    }
    Ok(())
}

fn parse<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<T> {
    serde_json::from_slice(b).map_err(|e| Error::new("XX001", format!("corrupt catalog: {e}")))
}

pub fn row_prefix(table_id: u32) -> Vec<u8> {
    let mut k = vec![ROW];
    k.extend_from_slice(&table_id.to_be_bytes());
    k
}

pub fn index_prefix(index_id: u32) -> Vec<u8> {
    let mut k = vec![INDEX];
    k.extend_from_slice(&index_id.to_be_bytes());
    k
}

impl Table {
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The encoded key of a row (primary key values), or None for row-id tables.
    pub fn key_of(&self, row: &[Value]) -> Option<Vec<u8>> {
        if self.primary_key.is_empty() {
            return None;
        }
        let mut k = Vec::new();
        for &c in &self.primary_key {
            row[c].encode_key(&mut k);
        }
        Some(k)
    }

    /// An index entry's key and value for a row stored under `row_key`.
    pub fn index_entry(&self, index: &Index, row: &[Value], row_key: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut k = index_prefix(index.id);
        let mut has_null = false;
        for &c in &index.columns {
            has_null |= row[c].is_null();
            row[c].encode_key(&mut k);
        }
        if !index.unique || has_null {
            k.extend_from_slice(row_key);
        }
        (k, row_key.to_vec())
    }
}

pub fn get_table(txn: &Transaction, name: &str) -> Result<Option<Table>> {
    get_blob(txn, &named(TABLE, name))?.map(|b| parse(&b)).transpose()
}

pub fn table(txn: &Transaction, name: &str) -> Result<Table> {
    get_table(txn, name)?.ok_or_else(|| Error::new("42P01", format!("relation \"{name}\" does not exist")))
}

pub fn put_table(txn: &Transaction, t: &Table) -> Result<()> {
    put_blob(txn, &named(TABLE, &t.name), &serde_json::to_vec(t).unwrap())
}

pub fn delete_table(txn: &Transaction, name: &str) -> Result<()> {
    delete_blob(txn, &named(TABLE, name))
}

pub fn tables(txn: &Transaction) -> Result<Vec<Table>> {
    let range = Range { prefix: &[TABLE], from: std::ops::Bound::Unbounded, to: std::ops::Bound::Unbounded };
    // Group the chunks by name (the bytes before the 0x00).
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (k, v) in txn.scan(&range, usize::MAX)? {
        let name = k[..k.iter().position(|&b| b == 0).unwrap_or(k.len())].to_vec();
        match out.last_mut() {
            Some((n, data)) if *n == name => data.extend_from_slice(&v),
            _ => out.push((name, v)),
        }
    }
    out.into_iter().map(|(_, b)| parse(&b)).collect()
}

pub fn get_view(txn: &Transaction, name: &str) -> Result<Option<View>> {
    get_blob(txn, &named(VIEW, name))?.map(|b| parse(&b)).transpose()
}

pub fn put_view(txn: &Transaction, v: &View) -> Result<()> {
    put_blob(txn, &named(VIEW, &v.name), &serde_json::to_vec(v).unwrap())
}

pub fn delete_view(txn: &Transaction, name: &str) -> Result<()> {
    delete_blob(txn, &named(VIEW, name))
}

/// Checks a row against the table's columns: converts each value to its column's type and
/// enforces NOT NULL.
pub fn conform(table: &Table, row: Row) -> Result<Row> {
    row.into_iter()
        .zip(&table.columns)
        .map(|(v, c)| {
            if v.is_null() {
                if !c.nullable {
                    return err!(
                        "23502",
                        "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                        c.name,
                        table.name
                    );
                }
                return Ok(Value::Null);
            }
            // As in PostgreSQL, a float stored in an integer column is rounded.
            v.cast(c.data_type)
        })
        .collect()
}
