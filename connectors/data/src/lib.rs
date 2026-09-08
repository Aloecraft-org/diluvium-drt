//! The `data` connector: `host:data/*` — columns out of a granted
//! directory, as raw buffers rather than as tables.
//!
//! The point of this connector is what it does *not* build. A million-row
//! `f64` column is a million Lua numbers if it arrives as a table, and a
//! table of a million strings if the column is text. So a column leaves
//! here as bytes on the reply's blob lane (`doc/Plan-2026-09.md` §3.2,
//! `drt_hostcall::column`), and a text column leaves dictionary-encoded:
//! `i64` codes on the lane, and one table holding each distinct string
//! once.
//!
//! The scope is `fs`'s scope, and the jail is `fs`'s jail
//! ([`drt_connector_fs::FsScope`]): the config grants a directory, the
//! program names files inside it, and a path resolving outside it is
//! refused with symlinks followed. Writing verbs need
//! `access = "readwrite"`, as they do there.
//!
//! ## surface block
//!
//! Entry points — the four verbs, dispatched in [`DataConnector::call`]:
//!
//! - `data/read_parquet` `{path, columns?, start?, len?}` -> [`read_parquet`]
//! - `data/write_parquet` `{path, columns, order?, compression?}` -> [`write_parquet`]
//! - `data/read_csv` `{path, dtypes?, header?, delimiter?}` -> [`read_csv`]
//! - `data/write_csv` `{path, columns, order?, header?}` -> [`write_csv`]
//!
//! Configurable values:
//!
//! - [`DEFAULT_MAX_BYTES`]: the file-size ceiling when the scope states none.
//! - [`DEFAULT_COMPRESSION`]: what `write_parquet` uses unasked.
//! - [`NULL_F64`]: the bits an `f64` column carries at a null row.
//! - [`codecs`]: the compression names accepted, and what each maps to.
//!   The fan-out point for compression; `brotli` is absent on purpose and
//!   the crate is not built with it.
//!
//! Fan-out points:
//!
//! - [`Column`]: the three shapes a column takes on the way out, and the
//!   only three. Every reader produces one of these and every writer
//!   consumes one.
//! - [`physical_to_column`]: parquet's physical types -> [`Column`]. A type
//!   not in that table is refused by name rather than approximated.
//! - [`Dtype`](drt_hostcall::Dtype): `f64`/`i64`/`u8`, the three `dv.h`
//!   fixes. Everything wider is widened into one of them at the edge, and
//!   everything else is refused.
//!
//! ## The answer's shape
//!
//! ```text
//! { rows = <n>,
//!   order = { "price", "name" },        -- schema order; a Lua map has none
//!   columns = {
//!     price = { values = <blob> },                       -- f64
//!     qty   = { values = <blob>, nulls = <blob> },        -- i64, some null
//!     name  = { codes  = <blob>, uniques = { "a", "b" } } -- text
//!   } }
//! ```
//!
//! `values`, `codes` and `valid` are blob descriptors — `{dtype, len,
//! blob}` — which a guest with `numeric` adopts as arrays and a guest
//! without reads as strings. `uniques` is a plain Lua list, so a code `c`
//! is `uniques[c + 1]`.
//!
//! ## Nulls
//!
//! Two representations, one per dtype, as the numeric spec's Stage 4
//! states them: **an `f64` column says null with NaN**, and **an `i64` or
//! text column carries an optional `u8` validity mask**, `valid`, with `1`
//! where the row has a value. A column with no null row carries no mask at
//! all, so the common case costs nothing.
//!
//! The `f64` rule has a consequence worth saying out loud: a NaN that was
//! data and a null are the same thing here, in both directions. Writing a
//! column whose row is NaN writes a null, and reading that row back gives
//! NaN. Nothing is lost on a round trip, and a column that needs to
//! distinguish the two wants an `i64` column with its own mask beside it.
//!
//! Either way a guest indexes by row: there is one slot per row even where
//! the row is null, so `values` and `codes` are never shorter than `rows`.

use std::sync::Arc;

use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};
use drt_connector_fs::{FsScope, FsScopeType};
use drt_hostcall::Dtype;
use drt_platform::fs::Backend;

use parquet::basic::{Compression, Encoding, Repetition, Type as PhysicalType};
use parquet::column::reader::ColumnReader;
use parquet::column::writer::ColumnWriter;
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int32Type, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type as SchemaType;

/// The file-size ceiling when the scope states none. `fs`'s number, because
/// it is the same question about the same directory — and a parquet file is
/// read whole into memory here, so this is also the memory bound.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// The bits an `f64` column carries at a null row: the canonical quiet
/// NaN, `0x7ff8000000000000`. Written from here rather than taken from the
/// platform, so every target produces the same eight bytes.
pub const NULL_F64: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf8, 0x7f];

/// What `write_parquet` compresses with when the call does not say. Snappy
/// rather than none: it is the cheapest of the four and a parquet file
/// nobody asked to be large should not be.
pub const DEFAULT_COMPRESSION: &str = "snappy";

/// The compression names this build accepts, and what each is.
///
/// Four codecs and no more, per `doc/Plan-2026-09.md` §5: `brotli` is not
/// here and the crate is not compiled with it, so a file that uses it is
/// refused at read with parquet's own message naming the codec rather than
/// silently mis-decoded.
pub fn codecs() -> Vec<(&'static str, Compression)> {
    vec![
        ("none", Compression::UNCOMPRESSED),
        ("snappy", Compression::SNAPPY),
        (
            "gzip",
            Compression::GZIP(parquet::basic::GzipLevel::default()),
        ),
        ("lz4", Compression::LZ4_RAW),
        (
            "zstd",
            Compression::ZSTD(parquet::basic::ZstdLevel::default()),
        ),
    ]
}

/// The three shapes a column takes crossing this boundary, and the only
/// three. `Text` is the one that exists so a million-row string column is
/// not a million Lua strings.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// Little-endian `f64`, one per row.
    F64(Vec<u8>),
    /// Little-endian `i64`, one per row. Narrower integers and booleans
    /// widen into this at the edge; nothing else does.
    I64(Vec<u8>),
    /// Dictionary-encoded text: `i64` codes, one per row, and each distinct
    /// string once.
    Text {
        codes: Vec<u8>,
        uniques: Vec<String>,
    },
}

impl Column {
    fn bytes(&self) -> &[u8] {
        match self {
            Column::F64(b) | Column::I64(b) => b,
            Column::Text { codes, .. } => codes,
        }
    }

    fn rows(&self) -> usize {
        self.bytes().len() / 8
    }
}

/// One decoded column and, when it needs one, its validity mask.
struct Decoded {
    name: String,
    column: Column,
    /// One byte per row, `1` where the row has a value. Empty when every
    /// row does -- and empty for an `f64` column always, because there the
    /// NaN in `values` is what says null.
    valid: Vec<u8>,
}

impl Decoded {
    /// The value a guest sees: the blob descriptors, plus `uniques` for a
    /// text column and `valid` for a non-`f64` column that has a null.
    fn into_value(self) -> rmpv::Value {
        let mut fields: Vec<(rmpv::Value, rmpv::Value)> = Vec::new();
        match self.column {
            Column::F64(bytes) => {
                fields.push(("values".into(), drt_hostcall::column(Dtype::F64, bytes)));
            }
            Column::I64(bytes) => {
                fields.push(("values".into(), drt_hostcall::column(Dtype::I64, bytes)));
            }
            Column::Text { codes, uniques } => {
                fields.push(("codes".into(), drt_hostcall::column(Dtype::I64, codes)));
                fields.push((
                    "uniques".into(),
                    rmpv::Value::Array(uniques.into_iter().map(rmpv::Value::from).collect()),
                ));
            }
        }
        if !self.valid.is_empty() {
            fields.push(("valid".into(), drt_hostcall::column(Dtype::U8, self.valid)));
        }
        rmpv::Value::Map(fields)
    }
}

/// Which [`Column`] a parquet physical type decodes into, or why it does
/// not decode at all.
///
/// The widening is deliberate and stated rather than implied: `dv.h` fixes
/// three dtypes, so a 32-bit column becomes 64-bit here and the guest never
/// sees a fourth width. `INT96` is a legacy timestamp with no lossless
/// 64-bit form and is refused rather than truncated; `FIXED_LEN_BYTE_ARRAY`
/// carries decimals and UUIDs whose meaning is in the logical type, and
/// guessing at it is worse than saying no.
pub fn physical_to_column(physical: PhysicalType) -> Result<Dtype, String> {
    match physical {
        PhysicalType::DOUBLE | PhysicalType::FLOAT => Ok(Dtype::F64),
        PhysicalType::INT64 | PhysicalType::INT32 | PhysicalType::BOOLEAN => Ok(Dtype::I64),
        PhysicalType::BYTE_ARRAY => Ok(Dtype::I64), // dictionary codes
        PhysicalType::INT96 => Err(
            "INT96 is a legacy timestamp with no lossless 64-bit form; rewrite the file with \
             a TIMESTAMP logical type"
                .into(),
        ),
        PhysicalType::FIXED_LEN_BYTE_ARRAY => Err(
            "FIXED_LEN_BYTE_ARRAY carries decimals and UUIDs whose width this connector cannot \
             guess; read it as BYTE_ARRAY or convert the column"
                .into(),
        ),
    }
}

// ---------------------------------------------------------------------------
// depth: parquet decoding
// ---------------------------------------------------------------------------

/// Read columns out of parquet bytes.
///
/// `wanted` is the column names to read, or all of them when empty; `start`
/// and `len` bound the rows. Row groups the range does not touch are never
/// decompressed, which is the whole reason the range is a parameter rather
/// than something the guest slices afterwards.
fn read_parquet_bytes(
    bytes: Vec<u8>,
    wanted: &[String],
    start: usize,
    len: Option<usize>,
) -> Result<(usize, Vec<Decoded>), String> {
    let reader = SerializedFileReader::new(bytes::Bytes::from(bytes))
        .map_err(|e| format!("not a readable parquet file: {e}"))?;
    let metadata = reader.metadata().clone();
    let schema = metadata.file_metadata().schema_descr();

    // Which leaf columns, in schema order, and where each sits.
    let mut chosen: Vec<(usize, String)> = Vec::new();
    for i in 0..schema.num_columns() {
        let name = schema.column(i).name().to_string();
        if wanted.is_empty() || wanted.contains(&name) {
            chosen.push((i, name));
        }
    }
    if !wanted.is_empty() {
        for want in wanted {
            if !chosen.iter().any(|(_, n)| n == want) {
                let have: Vec<String> = (0..schema.num_columns())
                    .map(|i| schema.column(i).name().to_string())
                    .collect();
                return Err(format!(
                    "no column named '{want}'; the file has {}",
                    have.join(", ")
                ));
            }
        }
    }

    let total: usize = metadata
        .row_groups()
        .iter()
        .map(|g| g.num_rows() as usize)
        .sum();
    let start = start.min(total);
    let take = len.unwrap_or(total - start).min(total - start);

    let mut out = Vec::with_capacity(chosen.len());
    for (index, name) in chosen {
        let physical = schema.column(index).physical_type();
        physical_to_column(physical).map_err(|why| format!("column '{name}': {why}"))?;
        let nullable = schema
            .column(index)
            .self_type()
            .get_basic_info()
            .repetition()
            != Repetition::REQUIRED;
        let mut acc = Accumulator::new(physical, nullable);

        // Walk the row groups the range touches and no others.
        let mut row = 0usize;
        for g in 0..metadata.num_row_groups() {
            let rows_here = metadata.row_group(g).num_rows() as usize;
            let group_end = row + rows_here;
            if group_end <= start || row >= start + take {
                row = group_end;
                continue;
            }
            let skip = start.saturating_sub(row);
            let want = (start + take).min(group_end) - row.max(start);
            let group = reader
                .get_row_group(g)
                .map_err(|e| format!("column '{name}': row group {g}: {e}"))?;
            let column = group
                .get_column_reader(index)
                .map_err(|e| format!("column '{name}': {e}"))?;
            acc.consume(column, skip, want)
                .map_err(|e| format!("column '{name}': {e}"))?;
            row = group_end;
        }
        out.push(acc.finish(name));
    }
    Ok((take, out))
}

/// Accumulates one column across the row groups a range touches.
///
/// Parquet hands back **only the non-null values**, densely, with a
/// definition level per row saying which rows had one. A guest indexing by
/// row cannot use that, so this re-expands: one slot per row, zero where
/// null, and a `nulls` byte beside it.
struct Accumulator {
    physical: PhysicalType,
    nullable: bool,
    values: Vec<u8>,
    /// One byte per row, `1` where the row was null. Turned into the
    /// validity mask -- or dropped, for `f64`, where the NaN says it -- by
    /// [`Accumulator::finish`].
    nulls: Vec<u8>,
    /// Text only: each distinct string once, and where it sits.
    uniques: Vec<String>,
    index: std::collections::HashMap<Vec<u8>, i64>,
}

impl Accumulator {
    fn new(physical: PhysicalType, nullable: bool) -> Self {
        Accumulator {
            physical,
            nullable,
            values: Vec::new(),
            nulls: Vec::new(),
            uniques: Vec::new(),
            index: std::collections::HashMap::new(),
        }
    }

    fn consume(
        &mut self,
        reader: ColumnReader,
        skip: usize,
        want: usize,
    ) -> Result<(), parquet::errors::ParquetError> {
        macro_rules! read {
            ($ty:ty, $reader:expr, $push:expr) => {{
                let mut typed = parquet::column::reader::get_typed_column_reader::<$ty>($reader);
                if skip > 0 {
                    typed.skip_records(skip)?;
                }
                let mut values = Vec::new();
                let mut defs: Vec<i16> = Vec::new();
                let mut read = 0usize;
                // In batches, because `read_records` stops at a page
                // boundary and answers with what it got.
                while read < want {
                    values.clear();
                    defs.clear();
                    let (records, _, _) = typed.read_records(
                        want - read,
                        if self.nullable { Some(&mut defs) } else { None },
                        None,
                        &mut values,
                    )?;
                    if records == 0 {
                        break;
                    }
                    self.expand(&values, &defs, records, $push);
                    read += records;
                }
                Ok(())
            }};
        }
        match self.physical {
            PhysicalType::DOUBLE => read!(DoubleType, reader, |v: &f64, out: &mut Vec<u8>| {
                out.extend_from_slice(&v.to_le_bytes())
            }),
            PhysicalType::FLOAT => read!(parquet::data_type::FloatType, reader, |v: &f32,
                                                                                 out: &mut Vec<
                u8,
            >| out
                .extend_from_slice(&(*v as f64).to_le_bytes())),
            PhysicalType::INT64 => read!(Int64Type, reader, |v: &i64, out: &mut Vec<u8>| {
                out.extend_from_slice(&v.to_le_bytes())
            }),
            PhysicalType::INT32 => read!(Int32Type, reader, |v: &i32, out: &mut Vec<u8>| {
                out.extend_from_slice(&(*v as i64).to_le_bytes())
            }),
            PhysicalType::BOOLEAN => read!(BoolType, reader, |v: &bool, out: &mut Vec<u8>| {
                out.extend_from_slice(&(*v as i64).to_le_bytes())
            }),
            PhysicalType::BYTE_ARRAY => {
                let mut typed =
                    parquet::column::reader::get_typed_column_reader::<ByteArrayType>(reader);
                if skip > 0 {
                    typed.skip_records(skip)?;
                }
                let mut values: Vec<ByteArray> = Vec::new();
                let mut defs: Vec<i16> = Vec::new();
                let mut read = 0usize;
                while read < want {
                    values.clear();
                    defs.clear();
                    let (records, _, _) = typed.read_records(
                        want - read,
                        if self.nullable { Some(&mut defs) } else { None },
                        None,
                        &mut values,
                    )?;
                    if records == 0 {
                        break;
                    }
                    // Interned here rather than after: the point of the
                    // dictionary is that a repeated string is stored once,
                    // and collecting first would defeat it.
                    let codes: Vec<i64> = values.iter().map(|v| self.intern(v.data())).collect();
                    self.expand(&codes, &defs, records, |c: &i64, out: &mut Vec<u8>| {
                        out.extend_from_slice(&c.to_le_bytes())
                    });
                    read += records;
                }
                Ok(())
            }
            // Refused before an accumulator was ever built.
            other => Err(parquet::errors::ParquetError::General(format!(
                "unreadable physical type {other}"
            ))),
        }
    }

    /// Turn parquet's dense non-null values plus definition levels into one
    /// slot per row, filling a null row with the dtype's null fill and
    /// recording that it was one.
    fn expand<T>(
        &mut self,
        values: &[T],
        defs: &[i16],
        records: usize,
        push: impl Fn(&T, &mut Vec<u8>),
    ) {
        if !self.nullable {
            for v in values.iter().take(records) {
                push(v, &mut self.values);
            }
            return;
        }
        // A max definition level of 1 for a flat optional column: 1 is
        // present, 0 is null. Flat is all this connector reads -- a nested
        // schema has a repetition level too, and no column shape to put it
        // in.
        let mut next = 0usize;
        let fill = self.null_fill();
        for def in defs.iter().take(records) {
            if *def > 0 {
                if let Some(v) = values.get(next) {
                    push(v, &mut self.values);
                    next += 1;
                    self.nulls.push(0);
                    continue;
                }
            }
            self.values.extend_from_slice(&fill);
            self.nulls.push(1);
        }
    }

    /// What sits in `values` at a null row.
    ///
    /// For `f64` it is a quiet NaN, and that NaN *is* the null; the bits
    /// are written from [`NULL_F64`] rather than taken from the platform,
    /// so every target produces the same eight bytes. For everything else
    /// it is zero and means nothing -- the validity mask says so.
    fn null_fill(&self) -> [u8; 8] {
        match self.physical {
            PhysicalType::DOUBLE | PhysicalType::FLOAT => NULL_F64,
            _ => [0u8; 8],
        }
    }

    fn intern(&mut self, bytes: &[u8]) -> i64 {
        if let Some(code) = self.index.get(bytes) {
            return *code;
        }
        let code = self.uniques.len() as i64;
        self.uniques
            .push(String::from_utf8_lossy(bytes).into_owned());
        self.index.insert(bytes.to_vec(), code);
        code
    }

    fn finish(self, name: String) -> Decoded {
        let column = match self.physical {
            PhysicalType::DOUBLE | PhysicalType::FLOAT => Column::F64(self.values),
            PhysicalType::BYTE_ARRAY => Column::Text {
                codes: self.values,
                uniques: self.uniques,
            },
            _ => Column::I64(self.values),
        };
        // An `f64` column carries no mask: the NaN in `values` is the
        // record. Every other column carries one only when it has a null,
        // because the common case is the one that should cost nothing.
        let valid = match &column {
            Column::F64(_) => Vec::new(),
            _ if self.nulls.contains(&1) => self.nulls.iter().map(|n| 1 - *n).collect(),
            _ => Vec::new(),
        };
        Decoded {
            name,
            column,
            valid,
        }
    }
}

// ---------------------------------------------------------------------------
// depth: parquet encoding
// ---------------------------------------------------------------------------

/// A column the guest handed back, ready to write.
///
/// The guest -> host direction has no blob lane: the lane is a property of
/// the *reply*, so a write carries its bytes inline in `args`, as a Lua
/// string. That is one copy on a path that is already doing file I/O, and
/// adding a request-side lane would be a second encoding change for a
/// smaller reason.
struct Writable {
    name: String,
    column: Column,
    /// One byte per row, `1` where the row is null. Empty when none is.
    ///
    /// Derived rather than taken: an `f64` column says null with NaN and
    /// carries no mask, so this is read off its values; every other column
    /// says it with `valid`, which is the inverse. Both are the read
    /// direction's rules run backwards, which is what makes a round trip a
    /// round trip.
    nulls: Vec<u8>,
}

fn write_parquet_bytes(columns: &[Writable], compression: Compression) -> Result<Vec<u8>, String> {
    let rows = columns.first().map(|c| c.column.rows()).unwrap_or(0);
    for c in columns {
        if c.column.rows() != rows {
            return Err(format!(
                "column '{}' has {} rows and the first has {rows}; a table's columns are the \
                 same length",
                c.name,
                c.column.rows()
            ));
        }
    }

    let fields: Vec<Arc<SchemaType>> = columns
        .iter()
        .map(|c| {
            let (physical, converted) = match &c.column {
                Column::F64(_) => (PhysicalType::DOUBLE, None),
                Column::I64(_) => (PhysicalType::INT64, None),
                Column::Text { .. } => (
                    PhysicalType::BYTE_ARRAY,
                    Some(parquet::basic::ConvertedType::UTF8),
                ),
            };
            let repetition = if c.nulls.is_empty() {
                Repetition::REQUIRED
            } else {
                Repetition::OPTIONAL
            };
            let mut builder =
                SchemaType::primitive_type_builder(&c.name, physical).with_repetition(repetition);
            if let Some(converted) = converted {
                builder = builder.with_converted_type(converted);
            }
            builder
                .build()
                .map(Arc::new)
                .map_err(|e| format!("column '{}': {e}", c.name))
        })
        .collect::<Result<_, _>>()?;

    let schema = Arc::new(
        SchemaType::group_type_builder("drt")
            .with_fields(fields)
            .build()
            .map_err(|e| format!("schema: {e}"))?,
    );
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(compression)
            // Plain rather than dictionary on the page: this connector's
            // text columns arrive already dictionary-encoded, so a second
            // dictionary would re-derive what the guest just handed over.
            .set_dictionary_enabled(false)
            .set_encoding(Encoding::PLAIN)
            .build(),
    );

    let mut buffer: Vec<u8> = Vec::new();
    {
        let mut writer = SerializedFileWriter::new(&mut buffer, schema, props)
            .map_err(|e| format!("cannot start the file: {e}"))?;
        let mut group = writer
            .next_row_group()
            .map_err(|e| format!("cannot start a row group: {e}"))?;
        for c in columns {
            let mut column = group
                .next_column()
                .map_err(|e| format!("column '{}': {e}", c.name))?
                .ok_or_else(|| format!("column '{}': the schema ran out of columns", c.name))?;
            write_one(column.untyped(), c).map_err(|e| format!("column '{}': {e}", c.name))?;
            column
                .close()
                .map_err(|e| format!("column '{}': {e}", c.name))?;
        }
        group
            .close()
            .map_err(|e| format!("cannot close the row group: {e}"))?;
        writer
            .close()
            .map_err(|e| format!("cannot close the file: {e}"))?;
    }
    Ok(buffer)
}

// depth: the dense-values-plus-levels shape, built rather than read
//
// The mirror of `Accumulator::expand`. Parquet takes only the values that
// are present, so a null row contributes a definition level and no value.
fn write_one(writer: &mut ColumnWriter<'_>, c: &Writable) -> Result<(), String> {
    let rows = c.column.rows();
    let defs: Option<Vec<i16>> = if c.nulls.is_empty() {
        None
    } else {
        Some(
            (0..rows)
                .map(|i| if c.nulls.get(i) == Some(&1) { 0 } else { 1 })
                .collect(),
        )
    };
    let present = |i: usize| defs.as_ref().is_none_or(|d| d[i] == 1);
    let bytes = c.column.bytes();
    let word = |i: usize| {
        let mut w = [0u8; 8];
        w.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
        w
    };
    let levels = defs.as_deref();

    let count = match (writer, &c.column) {
        (ColumnWriter::DoubleColumnWriter(w), Column::F64(_)) => {
            let values: Vec<f64> = (0..rows)
                .filter(|i| present(*i))
                .map(|i| f64::from_le_bytes(word(i)))
                .collect();
            w.write_batch(&values, levels, None)
        }
        (ColumnWriter::Int64ColumnWriter(w), Column::I64(_)) => {
            let values: Vec<i64> = (0..rows)
                .filter(|i| present(*i))
                .map(|i| i64::from_le_bytes(word(i)))
                .collect();
            w.write_batch(&values, levels, None)
        }
        (ColumnWriter::ByteArrayColumnWriter(w), Column::Text { uniques, .. }) => {
            let mut values: Vec<ByteArray> = Vec::new();
            for i in (0..rows).filter(|i| present(*i)) {
                let code = i64::from_le_bytes(word(i));
                let text = usize::try_from(code)
                    .ok()
                    .and_then(|c| uniques.get(c))
                    .ok_or_else(|| {
                        format!(
                            "row {i} has code {code}, and uniques has {} entries",
                            uniques.len()
                        )
                    })?;
                values.push(ByteArray::from(text.as_bytes().to_vec()));
            }
            w.write_batch(&values, levels, None)
        }
        _ => return Err("the column's bytes and the schema disagree".into()),
    };
    count.map(|_| ()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// depth: CSV
// ---------------------------------------------------------------------------

/// Split a CSV line, honouring `""` inside a quoted field. Not a general
/// CSV parser: no embedded newlines, which is the one shape this refuses,
/// by producing a row that does not parse rather than by guessing.
fn split_csv(line: &str, delimiter: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == delimiter && !quoted => out.push(std::mem::take(&mut field)),
            c => field.push(c),
        }
    }
    out.push(field);
    out
}

/// Read a CSV into columns.
///
/// `hints` names a column's dtype; a column with no hint is inferred from
/// what is in it, which is what a hint exists to override. An empty field
/// is a null, always: a CSV has no other way to say one, and it comes back
/// as NaN in an `f64` column and as a `valid` byte in any other.
fn read_csv_text(
    text: &str,
    hints: &std::collections::BTreeMap<String, String>,
    header: bool,
    delimiter: char,
) -> Result<(usize, Vec<Decoded>), String> {
    let mut lines = text.lines().filter(|l| !l.is_empty());
    let names: Vec<String> = match lines.next() {
        None => return Ok((0, Vec::new())),
        Some(first) if header => split_csv(first, delimiter),
        Some(first) => {
            let n = split_csv(first, delimiter).len();
            // No header: `c0`, `c1`, … and the first line is data, so it is
            // parsed again below rather than consumed here.
            let names = (0..n).map(|i| format!("c{i}")).collect();
            return read_csv_rows(
                text.lines().filter(|l| !l.is_empty()),
                names,
                hints,
                delimiter,
            );
        }
    };
    read_csv_rows(lines, names, hints, delimiter)
}

fn read_csv_rows<'a>(
    rows: impl Iterator<Item = &'a str>,
    names: Vec<String>,
    hints: &std::collections::BTreeMap<String, String>,
    delimiter: char,
) -> Result<(usize, Vec<Decoded>), String> {
    let mut cells: Vec<Vec<String>> = vec![Vec::new(); names.len()];
    let mut count = 0usize;
    for line in rows {
        let fields = split_csv(line, delimiter);
        for (i, cell) in cells.iter_mut().enumerate() {
            cell.push(fields.get(i).cloned().unwrap_or_default());
        }
        count += 1;
    }

    let mut out = Vec::with_capacity(names.len());
    for (name, cell) in names.into_iter().zip(cells) {
        let dtype = match hints.get(&name).map(|s| s.as_str()) {
            Some("f64") => "f64",
            Some("i64") => "i64",
            Some("str") => "str",
            Some(other) => {
                return Err(format!(
                    "dtypes.{name}: '{other}' is not one of f64, i64, str"
                ))
            }
            None => infer(&cell),
        };
        out.push(csv_column(name, cell, dtype)?);
    }
    Ok((count, out))
}

/// What a column looks like: an integer everywhere is `i64`, a number
/// everywhere is `f64`, anything else is text. An all-empty column is text,
/// because every one of its rows is null and nothing says otherwise.
fn infer(cells: &[String]) -> &'static str {
    let mut seen = false;
    let mut integral = true;
    for cell in cells.iter().filter(|c| !c.is_empty()) {
        seen = true;
        if cell.parse::<i64>().is_err() {
            integral = false;
            if cell.parse::<f64>().is_err() {
                return "str";
            }
        }
    }
    match (seen, integral) {
        (false, _) => "str",
        (true, true) => "i64",
        (true, false) => "f64",
    }
}

fn csv_column(name: String, cells: Vec<String>, dtype: &str) -> Result<Decoded, String> {
    let fill = if dtype == "f64" { NULL_F64 } else { [0u8; 8] };
    let mut nulls = Vec::with_capacity(cells.len());
    let mut values = Vec::with_capacity(cells.len() * 8);
    let mut uniques: Vec<String> = Vec::new();
    let mut index: std::collections::HashMap<String, i64> = std::collections::HashMap::new();

    for (row, cell) in cells.iter().enumerate() {
        if cell.is_empty() {
            nulls.push(1);
            values.extend_from_slice(&fill);
            continue;
        }
        nulls.push(0);
        match dtype {
            "f64" => {
                let v: f64 = cell
                    .parse()
                    .map_err(|_| format!("column '{name}' row {row}: '{cell}' is not a number"))?;
                values.extend_from_slice(&v.to_le_bytes());
            }
            "i64" => {
                let v: i64 = cell.parse().map_err(|_| {
                    format!("column '{name}' row {row}: '{cell}' is not an integer")
                })?;
                values.extend_from_slice(&v.to_le_bytes());
            }
            _ => {
                let code = match index.get(cell) {
                    Some(code) => *code,
                    None => {
                        let code = uniques.len() as i64;
                        uniques.push(cell.clone());
                        index.insert(cell.clone(), code);
                        code
                    }
                };
                values.extend_from_slice(&code.to_le_bytes());
            }
        }
    }

    let column = match dtype {
        "f64" => Column::F64(values),
        "i64" => Column::I64(values),
        _ => Column::Text {
            codes: values,
            uniques,
        },
    };
    let valid = match &column {
        Column::F64(_) => Vec::new(),
        _ if nulls.contains(&1) => nulls.iter().map(|n| 1 - *n).collect(),
        _ => Vec::new(),
    };
    Ok(Decoded {
        name,
        column,
        valid,
    })
}

fn write_csv_text(columns: &[Writable], header: bool, delimiter: char) -> Result<String, String> {
    let rows = columns.first().map(|c| c.column.rows()).unwrap_or(0);
    for c in columns {
        if c.column.rows() != rows {
            return Err(format!(
                "column '{}' has {} rows and the first has {rows}",
                c.name,
                c.column.rows()
            ));
        }
    }
    let mut out = String::new();
    if header {
        let names: Vec<String> = columns.iter().map(|c| quote(&c.name, delimiter)).collect();
        out.push_str(&names.join(&delimiter.to_string()));
        out.push('\n');
    }
    for row in 0..rows {
        let mut fields = Vec::with_capacity(columns.len());
        for c in columns {
            if c.nulls.get(row) == Some(&1) {
                // An empty field: the only thing a CSV can say for a null,
                // and what `read_csv` reads back as one.
                fields.push(String::new());
                continue;
            }
            let bytes = c.column.bytes();
            let mut w = [0u8; 8];
            w.copy_from_slice(&bytes[row * 8..row * 8 + 8]);
            fields.push(match &c.column {
                // %.17g is the shortest form that round-trips every double,
                // and a CSV a guest wrote should read back as what it wrote.
                Column::F64(_) => format!("{:?}", f64::from_le_bytes(w)),
                Column::I64(_) => i64::from_le_bytes(w).to_string(),
                Column::Text { uniques, .. } => {
                    let code = i64::from_le_bytes(w);
                    let text = usize::try_from(code)
                        .ok()
                        .and_then(|c| uniques.get(c))
                        .ok_or_else(|| {
                            format!(
                                "column '{}' row {row} has code {code}, and uniques has {} entries",
                                c.name,
                                uniques.len()
                            )
                        })?;
                    quote(text, delimiter)
                }
            });
        }
        out.push_str(&fields.join(&delimiter.to_string()));
        out.push('\n');
    }
    Ok(out)
}

fn quote(field: &str, delimiter: char) -> String {
    if field.contains(delimiter) || field.contains('"') || field.contains('\n') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

// ---------------------------------------------------------------------------
// The connector
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReadParquetArgs {
    path: String,
    /// Which columns; all of them when absent. Named rather than indexed,
    /// and a name the file does not have is refused with the file's own
    /// list rather than ignored.
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    start: Option<u64>,
    #[serde(default)]
    len: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ReadCsvArgs {
    path: String,
    #[serde(default)]
    dtypes: std::collections::BTreeMap<String, String>,
    #[serde(default = "yes")]
    header: bool,
    #[serde(default)]
    delimiter: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    /// name -> `{values | codes, uniques?, nulls?}`, the same shape a read
    /// answers with, so a guest round-trips a table without reshaping it.
    columns: rmpv::Value,
    /// Column order, since a Lua map has none. Absent means name order,
    /// which is at least stable.
    #[serde(default)]
    order: Vec<String>,
    #[serde(default)]
    compression: Option<String>,
    #[serde(default = "yes")]
    header: bool,
    #[serde(default)]
    delimiter: Option<String>,
}

fn yes() -> bool {
    true
}

/// One delimiter character, or why the argument is not one.
fn delimiter_of(given: Option<&String>) -> Result<char, String> {
    match given {
        None => Ok(','),
        Some(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(c),
                _ => Err(format!("delimiter must be one character (got {s:?})")),
            }
        }
    }
}

/// Read the guest's `columns` map back into something writable.
fn writables(value: rmpv::Value, order: &[String]) -> Result<Vec<Writable>, String> {
    let rmpv::Value::Map(entries) = value else {
        return Err("columns must be a table of name -> column".into());
    };
    let mut by_name: std::collections::BTreeMap<String, rmpv::Value> = Default::default();
    for (name, column) in entries {
        let name = name
            .as_str()
            .ok_or("a column name must be a string")?
            .to_string();
        by_name.insert(name, column);
    }
    let names: Vec<String> = if order.is_empty() {
        by_name.keys().cloned().collect()
    } else {
        for want in order {
            if !by_name.contains_key(want) {
                return Err(format!("order names '{want}', which columns does not have"));
            }
        }
        order.to_vec()
    };

    names
        .into_iter()
        .map(|name| {
            let value = by_name.remove(&name).expect("named above");
            let rmpv::Value::Map(fields) = value else {
                return Err(format!("column '{name}' must be a table"));
            };
            let field = |n: &str| {
                fields
                    .iter()
                    .find(|(k, _)| k.as_str() == Some(n))
                    .map(|(_, v)| v.clone())
            };
            let raw = |v: Option<rmpv::Value>| -> Option<Vec<u8>> {
                match v {
                    Some(rmpv::Value::Binary(b)) => Some(b),
                    Some(rmpv::Value::String(s)) => Some(s.into_bytes()),
                    _ => None,
                }
            };
            let valid = raw(field("valid")).unwrap_or_default();
            let column = match (raw(field("values")), raw(field("codes"))) {
                (Some(values), None) => {
                    // Which of the two eight-byte columns it is, said by the
                    // guest rather than guessed from the bytes -- they are
                    // indistinguishable, and guessing would silently write
                    // integers as doubles.
                    match field("dtype")
                        .and_then(|d| d.as_str().map(str::to_string))
                        .as_deref()
                    {
                        Some("f64") | None => Column::F64(values),
                        Some("i64") => Column::I64(values),
                        Some(other) => {
                            return Err(format!(
                                "column '{name}': dtype '{other}' is not f64 or i64"
                            ))
                        }
                    }
                }
                (None, Some(codes)) => {
                    let uniques = field("uniques")
                        .and_then(|u| u.as_array().map(|a| a.to_vec()))
                        .ok_or_else(|| {
                            format!("column '{name}': codes without uniques is not a text column")
                        })?
                        .into_iter()
                        .map(|v| {
                            v.as_str().map(str::to_string).ok_or_else(|| {
                                format!("column '{name}': uniques holds a non-string")
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Column::Text { codes, uniques }
                }
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "column '{name}' has both values and codes; a column is one or the other"
                    ))
                }
                (None, None) => {
                    return Err(format!("column '{name}' has neither values nor codes"))
                }
            };
            if column.bytes().len() % 8 != 0 {
                return Err(format!(
                    "column '{name}': {} bytes is not a whole number of eight-byte elements",
                    column.bytes().len()
                ));
            }
            let rows = column.rows();
            if !valid.is_empty() && valid.len() != rows {
                return Err(format!(
                    "column '{name}': valid has {} bytes and the column has {rows} rows",
                    valid.len()
                ));
            }
            let nulls = match &column {
                // NaN is the null, so the mask is read off the values --
                // and a `valid` beside them would be a second answer to a
                // question that already has one.
                Column::F64(bytes) => {
                    if !valid.is_empty() {
                        return Err(format!(
                            "column '{name}': an f64 column says null with NaN and takes no \
                             valid mask"
                        ));
                    }
                    let mask: Vec<u8> = bytes
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|w| u8::from(f64::from_le_bytes(*w).is_nan()))
                        .collect();
                    if mask.contains(&1) {
                        mask
                    } else {
                        Vec::new()
                    }
                }
                _ if valid.is_empty() => Vec::new(),
                _ => valid.iter().map(|v| 1 - v.min(&1)).collect(),
            };
            Ok(Writable {
                name,
                column,
                nulls,
            })
        })
        .collect()
}

/// The answer a read gives: `{rows, order, columns}`.
fn read_answer(rows: usize, decoded: Vec<Decoded>) -> rmpv::Value {
    let order: Vec<rmpv::Value> = decoded
        .iter()
        .map(|d| rmpv::Value::from(d.name.as_str()))
        .collect();
    let columns: Vec<(rmpv::Value, rmpv::Value)> = decoded
        .into_iter()
        .map(|d| (rmpv::Value::from(d.name.as_str()), d.into_value()))
        .collect();
    rmpv::Value::Map(vec![
        ("rows".into(), rmpv::Value::from(rows as u64)),
        ("order".into(), rmpv::Value::Array(order)),
        ("columns".into(), rmpv::Value::Map(columns)),
    ])
}

pub struct DataConnector {
    fs: Arc<dyn Backend>,
}

impl DataConnector {
    pub fn new() -> Self {
        DataConnector {
            fs: drt_platform::fs::host(),
        }
    }

    pub fn with_backend(fs: Arc<dyn Backend>) -> Self {
        DataConnector { fs }
    }
}

impl Default for DataConnector {
    fn default() -> Self {
        Self::new()
    }
}

/// The runtime this connector falls back to when the caller has none.
///
/// One per process, created on first use, never dropped: FM-1 is a
/// use-after-free in tokio's runtime teardown, and `rest`, `ssh`, `relay`,
/// `stun` and `tunnel` all leak theirs for that reason. Same mitigation,
/// spelled as ownership.
fn own_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("a tokio runtime for the data connector")
    })
}

/// Run a decode off the drive loop's thread.
///
/// **This is the shape of the connector**, not a detail of it. Decoding a
/// parquet file is unbounded CPU work, and doing it inline is `exec`'s
/// shape -- the drive loop stops, every other instance in the deployment
/// stops with it, and the guest's own instruction budget says nothing about
/// it because the work is not the guest's. On `spawn_blocking` the call
/// parks the way `rest` parks: the pump polls the future on its own
/// cadence, the answer lands when it lands, and nothing else in the
/// deployment waits.
async fn blocking<T, F>(work: F) -> Result<T, CallError>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let spawn = |handle: &tokio::runtime::Handle| handle.spawn_blocking(work);
    let joined = match tokio::runtime::Handle::try_current() {
        Ok(handle) => spawn(&handle).await,
        Err(_) => {
            let rt = own_runtime();
            let task = spawn(rt.handle());
            task.await
        }
    };
    joined
        .map_err(|e| CallError::new(format!("the decode did not finish: {e}")))?
        .map_err(CallError::new)
}

#[async_trait::async_trait]
impl Connector for DataConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(FsScopeType::new(self.fs.clone()))
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let sc = FsScope::parse(scope).map_err(CallError::new)?;
        let args =
            args.ok_or_else(|| CallError::new(format!("{call} takes args, and got none")))?;
        let writing = matches!(call, "data/write_parquet" | "data/write_csv");
        if writing && !sc.writable() {
            return Err(CallError::new(format!(
                "'{call}' needs access = \"readwrite\"; this scope is read-only"
            )));
        }

        match call {
            "data/read_parquet" => {
                let a: ReadParquetArgs = rmpv::ext::from_value(args)
                    .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
                let bytes = self.slurp(&sc, &a.path)?;
                let start = a.start.unwrap_or(0) as usize;
                let len = a.len.map(|l| l as usize);
                let columns = a.columns.clone();
                let (rows, decoded) =
                    blocking(move || read_parquet_bytes(bytes, &columns, start, len)).await?;
                Ok(read_answer(rows, decoded))
            }
            "data/read_csv" => {
                let a: ReadCsvArgs = rmpv::ext::from_value(args)
                    .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
                let delimiter = delimiter_of(a.delimiter.as_ref()).map_err(CallError::new)?;
                let bytes = self.slurp(&sc, &a.path)?;
                let hints = a.dtypes.clone();
                let header = a.header;
                let (rows, decoded) = blocking(move || {
                    let text = String::from_utf8(bytes)
                        .map_err(|_| "the file is not UTF-8 text".to_string())?;
                    read_csv_text(&text, &hints, header, delimiter)
                })
                .await?;
                Ok(read_answer(rows, decoded))
            }
            "data/write_parquet" => {
                let a: WriteArgs = rmpv::ext::from_value(args)
                    .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
                let name = a.compression.clone().unwrap_or(DEFAULT_COMPRESSION.into());
                let codec = codecs()
                    .into_iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, c)| c)
                    .ok_or_else(|| {
                        let names: Vec<&str> = codecs().into_iter().map(|(n, _)| n).collect();
                        CallError::new(format!(
                            "compression '{name}' is not one of {}",
                            names.join(", ")
                        ))
                    })?;
                let columns = writables(a.columns, &a.order).map_err(CallError::new)?;
                let rows = columns.first().map(|c| c.column.rows()).unwrap_or(0);
                let bytes = blocking(move || write_parquet_bytes(&columns, codec)).await?;
                self.spill(&sc, &a.path, &bytes)?;
                Ok(written(rows, bytes.len()))
            }
            "data/write_csv" => {
                let a: WriteArgs = rmpv::ext::from_value(args)
                    .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
                let delimiter = delimiter_of(a.delimiter.as_ref()).map_err(CallError::new)?;
                let columns = writables(a.columns, &a.order).map_err(CallError::new)?;
                let rows = columns.first().map(|c| c.column.rows()).unwrap_or(0);
                let header = a.header;
                let text = blocking(move || write_csv_text(&columns, header, delimiter)).await?;
                self.spill(&sc, &a.path, text.as_bytes())?;
                Ok(written(rows, text.len()))
            }
            other => Err(CallError::new(format!(
                "'{other}' is not a data call (read_parquet, write_parquet, read_csv, write_csv)"
            ))),
        }
    }
}

fn written(rows: usize, bytes: usize) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("rows".into(), rmpv::Value::from(rows as u64)),
        ("bytes".into(), rmpv::Value::from(bytes as u64)),
    ])
}

impl DataConnector {
    /// Read a file inside the scope, refusing past `max_bytes`.
    ///
    /// Whole, into memory: parquet's reader wants random access to the
    /// footer and then to the row groups the range touches, and a guest
    /// cannot bound the file's size, so `max_bytes` is both the file bound
    /// and the memory bound. Same number and same reasoning as `fs`.
    fn slurp(&self, sc: &FsScope, path: &str) -> Result<Vec<u8>, CallError> {
        let resolved = sc.resolve(&*self.fs, path, true).map_err(CallError::new)?;
        let meta = self
            .fs
            .metadata(&resolved)
            .map_err(|e| CallError::new(format!("'{path}': {e}")))?;
        if meta.len > sc.max_bytes() {
            return Err(CallError::new(format!(
                "'{path}' is {} bytes and this scope's max_bytes is {}",
                meta.len,
                sc.max_bytes()
            )));
        }
        self.fs
            .read(&resolved)
            .map_err(|e| CallError::new(format!("'{path}': {e}")))
    }

    fn spill(&self, sc: &FsScope, path: &str, bytes: &[u8]) -> Result<(), CallError> {
        if bytes.len() as u64 > sc.max_bytes() {
            return Err(CallError::new(format!(
                "'{path}' would be {} bytes and this scope's max_bytes is {}",
                bytes.len(),
                sc.max_bytes()
            )));
        }
        let resolved = sc.resolve(&*self.fs, path, false).map_err(CallError::new)?;
        self.fs
            .write(&resolved, bytes, false)
            .map_err(|e| CallError::new(format!("'{path}': {e}")))
    }
}
