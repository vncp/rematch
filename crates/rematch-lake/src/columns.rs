//! Flat Arrow columns built from JSON (ADR 001 §"Tables and keys"): scalars, `List<Utf8>`
//! (Bellwether refuses lists of structs) and `Timestamp(ms, UTC)` time columns.

use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{
    ArrayRef, BooleanBuilder, Float64Builder, Int64Builder, ListBuilder, StringBuilder,
    TimestampMillisecondBuilder,
};
use arrow::datatypes::{DataType, Field, FieldRef, Schema, SchemaRef, TimeUnit};
use arrow::record_batch::RecordBatch;
use serde_json::Value;

/// Timezone of every time column.
pub const UTC: &str = "UTC";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Int64,
    Float64,
    Bool,
    Utf8,
    /// `Timestamp(Millisecond, "UTC")`.
    TsMs,
    /// `List<Utf8>` with non-null items.
    ListUtf8,
}

/// One column of a table: name, type, and whether nulls are allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Col {
    pub name: &'static str,
    pub ty: Ty,
    pub nullable: bool,
}

/// Non-null column.
pub const fn req(name: &'static str, ty: Ty) -> Col {
    Col { name, ty, nullable: false }
}

/// Nullable column.
pub const fn opt(name: &'static str, ty: Ty) -> Col {
    Col { name, ty, nullable: true }
}

/// One value of one row; [`Cell::Null`] in a non-null column becomes the type's zero value.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Null,
    I(i64),
    F(f64),
    B(bool),
    S(String),
    L(Vec<String>),
}

fn list_item() -> FieldRef {
    Arc::new(Field::new("item", DataType::Utf8, false))
}

impl Ty {
    pub fn data_type(self) -> DataType {
        match self {
            Ty::Int64 => DataType::Int64,
            Ty::Float64 => DataType::Float64,
            Ty::Bool => DataType::Boolean,
            Ty::Utf8 => DataType::Utf8,
            Ty::TsMs => DataType::Timestamp(TimeUnit::Millisecond, Some(UTC.into())),
            Ty::ListUtf8 => DataType::List(list_item()),
        }
    }
}

pub fn schema(cols: &[Col]) -> SchemaRef {
    Arc::new(Schema::new(
        cols.iter().map(|c| Field::new(c.name, c.ty.data_type(), c.nullable)).collect::<Vec<_>>(),
    ))
}

/// Column-major build of `rows` (each aligned with `cols`) into one batch.
pub fn batch(cols: &[Col], rows: &[Vec<Cell>]) -> Result<RecordBatch> {
    let arrays: Vec<ArrayRef> = cols.iter().enumerate().map(|(i, c)| array(c, rows.iter().map(|r| &r[i]))).collect();
    RecordBatch::try_new(schema(cols), arrays).context("build record batch")
}

fn array<'a>(c: &Col, cells: impl Iterator<Item = &'a Cell>) -> ArrayRef {
    let nullable = c.nullable;
    match c.ty {
        Ty::Int64 => {
            let mut b = Int64Builder::new();
            for x in cells {
                match x {
                    Cell::I(v) => b.append_value(*v),
                    _ if nullable => b.append_null(),
                    _ => b.append_value(0),
                }
            }
            Arc::new(b.finish())
        }
        Ty::TsMs => {
            let mut b = TimestampMillisecondBuilder::new().with_timezone(UTC);
            for x in cells {
                match x {
                    Cell::I(v) => b.append_value(*v),
                    _ if nullable => b.append_null(),
                    _ => b.append_value(0),
                }
            }
            Arc::new(b.finish())
        }
        Ty::Float64 => {
            let mut b = Float64Builder::new();
            for x in cells {
                match x {
                    Cell::F(v) => b.append_value(*v),
                    Cell::I(v) => b.append_value(*v as f64),
                    _ if nullable => b.append_null(),
                    _ => b.append_value(0.0),
                }
            }
            Arc::new(b.finish())
        }
        Ty::Bool => {
            let mut b = BooleanBuilder::new();
            for x in cells {
                match x {
                    Cell::B(v) => b.append_value(*v),
                    _ if nullable => b.append_null(),
                    _ => b.append_value(false),
                }
            }
            Arc::new(b.finish())
        }
        Ty::Utf8 => {
            let mut b = StringBuilder::new();
            for x in cells {
                match x {
                    Cell::S(v) => b.append_value(v),
                    _ if nullable => b.append_null(),
                    _ => b.append_value(""),
                }
            }
            Arc::new(b.finish())
        }
        Ty::ListUtf8 => {
            let mut b = ListBuilder::new(StringBuilder::new()).with_field(list_item());
            for x in cells {
                match x {
                    Cell::L(items) => {
                        for s in items {
                            b.values().append_value(s);
                        }
                        b.append(true);
                    }
                    _ if nullable => b.append(false),
                    _ => b.append(true),
                }
            }
            Arc::new(b.finish())
        }
    }
}

// ---------------- JSON → cell ----------------

/// A JSON string; any other non-null scalar is not a string column value (`Null`).
pub fn s(v: Option<&Value>) -> Cell {
    match v {
        Some(Value::String(x)) => Cell::S(x.clone()),
        _ => Cell::Null,
    }
}

pub fn i(v: Option<&Value>) -> Cell {
    match v.and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|u| u as i64))) {
        Some(n) => Cell::I(n),
        None => Cell::Null,
    }
}

pub fn f(v: Option<&Value>) -> Cell {
    match v.and_then(Value::as_f64) {
        Some(x) => Cell::F(x),
        None => Cell::Null,
    }
}

pub fn b(v: Option<&Value>) -> Cell {
    match v.and_then(Value::as_bool) {
        Some(x) => Cell::B(x),
        None => Cell::Null,
    }
}

/// An array of strings (non-string items as their JSON text); `Null` when absent or not an array.
pub fn l(v: Option<&Value>) -> Cell {
    match v {
        Some(Value::Array(items)) => Cell::L(
            items
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        ),
        _ => Cell::Null,
    }
}

/// The value as compact JSON text; `Null` when absent or JSON null.
pub fn js(v: Option<&Value>) -> Cell {
    match v {
        None | Some(Value::Null) => Cell::Null,
        Some(x) => Cell::S(x.to_string()),
    }
}

/// Whether `cell` is what a column of type `ty` holds (used to route a mistyped trace
/// field into `extra` instead of dropping it).
pub fn fits(ty: Ty, cell: &Cell) -> bool {
    matches!(
        (ty, cell),
        (Ty::Int64 | Ty::TsMs, Cell::I(_))
            | (Ty::Float64, Cell::F(_) | Cell::I(_))
            | (Ty::Bool, Cell::B(_))
            | (Ty::Utf8, Cell::S(_))
            | (Ty::ListUtf8, Cell::L(_))
    )
}
