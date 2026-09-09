//! What the `data` connector promises: columns cross as bytes, text
//! crosses as a dictionary, nulls are said rather than implied, and the
//! jail is `fs`'s jail.
//!
//! ## surface block
//!
//! - [`call`]: the only way these tests reach the connector, so every one
//!   of them goes through the real dispatch path including the scope check.
//! - [`SCOPE`], [`RW`]: the two scope shapes, read-only and writable.
//! - [`f64s`], [`i64s`], [`bytes_of`], [`blob`]: reading an answer apart.
//!
//! Every assertion about a numeric value is against its **bits**
//! (`doc/Plan-2026-09.md` §3.3): the question these tests ask is whether
//! exact bytes crossed, and a formatted double is a different question with
//! four answers.

use drt_caps::Scope;
use drt_connector::{CallError, Connector};
use drt_connector_data::DataConnector;

/// A read-only scope over `dir`.
fn scope(dir: &std::path::Path) -> Scope {
    Scope(rmpv::Value::Map(vec![(
        "scope".into(),
        rmpv::Value::from(dir.to_str().unwrap()),
    )]))
}

/// The same, writable, with room for the files these tests write.
fn rw(dir: &std::path::Path) -> Scope {
    Scope(rmpv::Value::Map(vec![
        ("scope".into(), rmpv::Value::from(dir.to_str().unwrap())),
        ("access".into(), rmpv::Value::from("readwrite")),
        ("max_bytes".into(), rmpv::Value::from(4_000_000u64)),
    ]))
}

fn call(sc: &Scope, name: &str, args: Vec<(&str, rmpv::Value)>) -> Result<rmpv::Value, CallError> {
    let connector = DataConnector::new();
    let args = rmpv::Value::Map(
        args.into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    );
    pollster::block_on(connector.call(name, Some(args), Some(sc)))
}

/// The bytes of a column, whichever of the three fields carries it. The
/// connector answers a `column` ext value; these tests read it directly,
/// which is what the dispatcher's `lift_columns` would otherwise do.
fn bytes_of(column: &rmpv::Value, field: &str) -> Vec<u8> {
    let fields = column.as_map().expect("a column is a table");
    let value = &fields
        .iter()
        .find(|(k, _)| k.as_str() == Some(field))
        .unwrap_or_else(|| panic!("the column has no '{field}': {column:?}"))
        .1;
    match value {
        rmpv::Value::Ext(_, bytes) => bytes.clone(),
        other => panic!("'{field}' is not a column: {other:?}"),
    }
}

fn has(column: &rmpv::Value, field: &str) -> bool {
    column
        .as_map()
        .expect("a column is a table")
        .iter()
        .any(|(k, _)| k.as_str() == Some(field))
}

fn f64s(column: &rmpv::Value) -> Vec<f64> {
    bytes_of(column, "values")
        .as_chunks::<8>()
        .0
        .iter()
        .map(|w| f64::from_le_bytes(*w))
        .collect()
}

fn i64s(column: &rmpv::Value, field: &str) -> Vec<i64> {
    bytes_of(column, field)
        .as_chunks::<8>()
        .0
        .iter()
        .map(|w| i64::from_le_bytes(*w))
        .collect()
}

fn column<'a>(answer: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    let fields = answer.as_map().expect("the answer is a table");
    let columns = &fields
        .iter()
        .find(|(k, _)| k.as_str() == Some("columns"))
        .expect("the answer has columns")
        .1;
    &columns
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .unwrap_or_else(|| panic!("no column '{name}' in {answer:?}"))
        .1
}

fn rows(answer: &rmpv::Value) -> u64 {
    answer
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some("rows"))
        .unwrap()
        .1
        .as_u64()
        .unwrap()
}

fn uniques(column: &rmpv::Value) -> Vec<String> {
    column
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some("uniques"))
        .expect("a text column has uniques")
        .1
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// An f64 column, as a guest hands one over: the raw little-endian bytes.
fn f64_column(values: &[f64]) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("dtype".into(), rmpv::Value::from("f64")),
        (
            "values".into(),
            rmpv::Value::Binary(values.iter().flat_map(|v| v.to_le_bytes()).collect()),
        ),
    ])
}

fn i64_column(values: &[i64]) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("dtype".into(), rmpv::Value::from("i64")),
        (
            "values".into(),
            rmpv::Value::Binary(values.iter().flat_map(|v| v.to_le_bytes()).collect()),
        ),
    ])
}

fn text_column(codes: &[i64], uniques: &[&str]) -> rmpv::Value {
    rmpv::Value::Map(vec![
        (
            "codes".into(),
            rmpv::Value::Binary(codes.iter().flat_map(|v| v.to_le_bytes()).collect()),
        ),
        (
            "uniques".into(),
            rmpv::Value::Array(uniques.iter().map(|u| rmpv::Value::from(*u)).collect()),
        ),
    ])
}

fn table(columns: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        columns
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

// ---------------------------------------------------------------------------

/// The round trip: what a guest writes is bit-for-bit what it reads back.
///
/// The doubles are chosen to break a decimal round trip that is not exact:
/// 0.1 has no finite binary form, and a writer that went through a decimal
/// string on the way out would come back with different bits.
#[test]
fn a_parquet_round_trip_returns_the_same_bits() {
    let dir = tempfile::tempdir().unwrap();
    let values = [0.1f64, -0.0, 2.5, 1e308, f64::MIN_POSITIVE];
    call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            (
                "columns",
                table(vec![
                    ("price", f64_column(&values)),
                    ("qty", i64_column(&[1, -2, 3, -4, 5])),
                ]),
            ),
            (
                "order",
                rmpv::Value::Array(vec!["price".into(), "qty".into()]),
            ),
        ],
    )
    .expect("the write succeeded");

    let answer = call(
        &scope(dir.path()),
        "data/read_parquet",
        vec![("path", rmpv::Value::from("t.parquet"))],
    )
    .expect("the read succeeded");

    assert_eq!(rows(&answer), 5);
    let price = column(&answer, "price");
    assert_eq!(
        f64s(price).iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        "a double did not survive the round trip"
    );
    assert_eq!(i64s(column(&answer, "qty"), "values"), [1, -2, 3, -4, 5]);
    // No null anywhere, so no mask: the common case costs nothing.
    assert!(
        !has(price, "valid"),
        "a column with no nulls carries no mask"
    );
    assert!(!has(column(&answer, "qty"), "valid"));
}

/// Text crosses as a dictionary, never as a table of strings. A repeated
/// value is stored once, and the codes index it.
#[test]
fn a_text_column_crosses_as_codes_and_uniques() {
    let dir = tempfile::tempdir().unwrap();
    call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            (
                "columns",
                table(vec![(
                    "city",
                    text_column(&[0, 1, 0, 0, 1], &["oslo", "bergen"]),
                )]),
            ),
        ],
    )
    .expect("the write succeeded");

    let answer = call(
        &scope(dir.path()),
        "data/read_parquet",
        vec![("path", rmpv::Value::from("t.parquet"))],
    )
    .expect("the read succeeded");

    let city = column(&answer, "city");
    // Five rows, two distinct strings: the whole point.
    assert_eq!(uniques(city).len(), 2);
    let codes = i64s(city, "codes");
    assert_eq!(codes.len(), 5);
    let names: Vec<&str> = codes
        .iter()
        .map(|c| uniques(city)[*c as usize].clone())
        .map(|s| Box::leak(s.into_boxed_str()) as &str)
        .collect();
    assert_eq!(names, ["oslo", "bergen", "oslo", "oslo", "bergen"]);
}

/// The two null representations the numeric spec's Stage 4 fixes: an `f64`
/// column says null with NaN and carries no mask; every other column
/// carries a `u8` validity mask, `1` where the row has a value.
///
/// Both directions, in one round trip: the NaN a guest writes comes back as
/// a NaN, and the `valid` it writes comes back as a `valid`.
#[test]
fn an_f64_says_null_with_nan_and_everything_else_with_a_mask() {
    let dir = tempfile::tempdir().unwrap();
    let mut coded = text_column(&[0, 0, 1], &["oslo", "bergen"]);
    if let rmpv::Value::Map(fields) = &mut coded {
        fields.push(("valid".into(), rmpv::Value::Binary(vec![1, 0, 1])));
    }
    let mut qty = i64_column(&[7, 0, 9]);
    if let rmpv::Value::Map(fields) = &mut qty {
        fields.push(("valid".into(), rmpv::Value::Binary(vec![1, 0, 1])));
    }
    call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            (
                "columns",
                table(vec![
                    ("price", f64_column(&[1.0, f64::NAN, 3.0])),
                    ("qty", qty),
                    ("city", coded),
                ]),
            ),
            (
                "order",
                rmpv::Value::Array(vec!["price".into(), "qty".into(), "city".into()]),
            ),
        ],
    )
    .expect("the write succeeded");

    let answer = call(
        &scope(dir.path()),
        "data/read_parquet",
        vec![("path", rmpv::Value::from("t.parquet"))],
    )
    .expect("the read succeeded");
    assert_eq!(rows(&answer), 3);

    // f64: the NaN is the null, and there is no mask to carry it.
    let price = column(&answer, "price");
    assert!(
        !has(price, "valid"),
        "an f64 column carries no validity mask"
    );
    let values = f64s(price);
    assert_eq!(values[0].to_bits(), 1.0f64.to_bits());
    assert!(
        values[1].is_nan(),
        "the null row came back as {:?}",
        values[1]
    );
    // The exact bits, not just "a NaN": written from the connector's own
    // constant so every target produces the same eight bytes.
    assert_eq!(values[1].to_bits(), 0x7ff8_0000_0000_0000);
    assert_eq!(values[2].to_bits(), 3.0f64.to_bits());

    // i64 and text: a validity mask, `1` where there is a value.
    assert_eq!(bytes_of(column(&answer, "qty"), "valid"), vec![1, 0, 1]);
    assert_eq!(bytes_of(column(&answer, "city"), "valid"), vec![1, 0, 1]);
    // One slot per row even where null, so a guest indexes by row.
    assert_eq!(i64s(column(&answer, "qty"), "values").len(), 3);
    assert_eq!(i64s(column(&answer, "city"), "codes").len(), 3);
}

/// A `valid` mask on an `f64` column is two answers to a question that has
/// one, and is refused rather than silently ignored.
#[test]
fn an_f64_column_does_not_take_a_validity_mask() {
    let dir = tempfile::tempdir().unwrap();
    let mut price = f64_column(&[1.0, 2.0]);
    if let rmpv::Value::Map(fields) = &mut price {
        fields.push(("valid".into(), rmpv::Value::Binary(vec![1, 0])));
    }
    let err = call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            ("columns", table(vec![("price", price)])),
        ],
    )
    .unwrap_err();
    assert!(err.0.contains("NaN"), "got: {}", err.0);
}

/// Columns and a row range are the connector's arguments, not something a
/// guest slices afterwards: naming them is what stops the other columns
/// being decompressed at all.
#[test]
fn a_read_takes_the_columns_and_the_rows_it_was_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let a: Vec<f64> = (0..100).map(|i| i as f64).collect();
    let b: Vec<i64> = (0..100).collect();
    call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            (
                "columns",
                table(vec![("a", f64_column(&a)), ("b", i64_column(&b))]),
            ),
        ],
    )
    .unwrap();

    let answer = call(
        &scope(dir.path()),
        "data/read_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            ("columns", rmpv::Value::Array(vec!["b".into()])),
            ("start", rmpv::Value::from(10u64)),
            ("len", rmpv::Value::from(5u64)),
        ],
    )
    .expect("the read succeeded");

    assert_eq!(rows(&answer), 5);
    assert_eq!(i64s(column(&answer, "b"), "values"), [10, 11, 12, 13, 14]);
    // And only what was asked for.
    let names: Vec<String> = answer
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some("order"))
        .unwrap()
        .1
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["b"]);
}

/// A column the file does not have is refused with the file's own list, so
/// the next call can be right rather than another guess.
#[test]
fn an_unknown_column_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            ("columns", table(vec![("a", f64_column(&[1.0]))])),
        ],
    )
    .unwrap();
    let err = call(
        &scope(dir.path()),
        "data/read_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            ("columns", rmpv::Value::Array(vec!["nope".into()])),
        ],
    )
    .unwrap_err();
    assert!(err.0.contains("no column named 'nope'"), "got: {}", err.0);
    assert!(
        err.0.contains('a'),
        "the message names what the file has: {}",
        err.0
    );
}

/// Every codec the plan names round-trips, and `brotli` is not one of them
/// because this build does not carry it.
#[test]
fn the_four_codecs_round_trip_and_brotli_is_not_offered() {
    let dir = tempfile::tempdir().unwrap();
    for codec in ["none", "snappy", "gzip", "lz4", "zstd"] {
        let path = format!("{codec}.parquet");
        call(
            &rw(dir.path()),
            "data/write_parquet",
            vec![
                ("path", rmpv::Value::from(path.as_str())),
                ("columns", table(vec![("a", f64_column(&[1.5, 2.5]))])),
                ("compression", rmpv::Value::from(codec)),
            ],
        )
        .unwrap_or_else(|e| panic!("{codec}: {}", e.0));
        let answer = call(
            &scope(dir.path()),
            "data/read_parquet",
            vec![("path", rmpv::Value::from(path.as_str()))],
        )
        .unwrap_or_else(|e| panic!("{codec}: {}", e.0));
        assert_eq!(
            f64s(column(&answer, "a"))
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            [1.5f64.to_bits(), 2.5f64.to_bits()],
            "{codec} did not round-trip"
        );
    }

    let err = call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("b.parquet")),
            ("columns", table(vec![("a", f64_column(&[1.0]))])),
            ("compression", rmpv::Value::from("brotli")),
        ],
    )
    .unwrap_err();
    assert!(err.0.contains("brotli"), "got: {}", err.0);
    assert!(
        err.0.contains("snappy"),
        "the refusal names what is offered: {}",
        err.0
    );
}

/// CSV: a hint decides a column's type, and a column with no hint is read
/// from what is in it.
#[test]
fn csv_takes_a_dtype_hint_and_infers_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("t.csv"),
        "id,price,city,code\n1,0.5,oslo,007\n2,1.5,bergen,008\n3,,oslo,009\n",
    )
    .unwrap();

    let answer = call(
        &scope(dir.path()),
        "data/read_csv",
        vec![
            ("path", rmpv::Value::from("t.csv")),
            // `code` looks like an integer and is not one: a leading zero
            // is data. That is what a hint is for.
            (
                "dtypes",
                rmpv::Value::Map(vec![("code".into(), rmpv::Value::from("str"))]),
            ),
        ],
    )
    .expect("the read succeeded");

    assert_eq!(rows(&answer), 3);
    assert_eq!(i64s(column(&answer, "id"), "values"), [1, 2, 3]);
    assert_eq!(uniques(column(&answer, "city")), ["oslo", "bergen"]);
    assert_eq!(uniques(column(&answer, "code")), ["007", "008", "009"]);
    // An empty field is a null, which is the only thing a CSV can say --
    // and in an f64 column that is a NaN, not a mask.
    let price = column(&answer, "price");
    assert!(!has(price, "valid"));
    assert_eq!(f64s(price)[0].to_bits(), 0.5f64.to_bits());
    assert!(f64s(price)[2].is_nan());
    // In an i64 column it is a mask. `id` has no empty field, so none.
    assert!(!has(column(&answer, "id"), "valid"));
}

/// A CSV a guest writes reads back as what it wrote, doubles included.
#[test]
fn a_csv_round_trip_returns_the_same_bits() {
    let dir = tempfile::tempdir().unwrap();
    let values = [0.1f64, 1.0 / 3.0, 1e-300];
    call(
        &rw(dir.path()),
        "data/write_csv",
        vec![
            ("path", rmpv::Value::from("t.csv")),
            (
                "columns",
                table(vec![
                    ("v", f64_column(&values)),
                    ("name", text_column(&[0, 1, 0], &["a,b", "plain"])),
                ]),
            ),
            ("order", rmpv::Value::Array(vec!["v".into(), "name".into()])),
        ],
    )
    .expect("the write succeeded");

    let answer = call(
        &scope(dir.path()),
        "data/read_csv",
        vec![("path", rmpv::Value::from("t.csv"))],
    )
    .expect("the read succeeded");

    assert_eq!(
        f64s(column(&answer, "v"))
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        "a double did not survive the text round trip"
    );
    // A field holding the delimiter is quoted on the way out and unquoted
    // on the way back, rather than splitting the row.
    assert_eq!(uniques(column(&answer, "name")), ["a,b", "plain"]);
}

/// The jail is `fs`'s jail: a path out of the granted directory is refused,
/// and a read-only scope refuses the writing verbs.
#[test]
fn the_scope_is_a_place_and_the_program_names_files_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.csv"), "a\n1\n").unwrap();

    for path in ["../escape.csv", "/etc/passwd"] {
        let err = call(
            &scope(dir.path()),
            "data/read_csv",
            vec![("path", rmpv::Value::from(path))],
        )
        .unwrap_err();
        assert!(
            err.0.contains("outside the granted scope") || err.0.contains("absolute"),
            "{path} was not refused: {}",
            err.0
        );
    }

    let err = call(
        &scope(dir.path()),
        "data/write_csv",
        vec![
            ("path", rmpv::Value::from("out.csv")),
            ("columns", table(vec![("a", f64_column(&[1.0]))])),
        ],
    )
    .unwrap_err();
    assert!(err.0.contains("readwrite"), "got: {}", err.0);

    // And no scope at all is a refusal, not a default place.
    let connector = DataConnector::new();
    let err = pollster::block_on(connector.call(
        "data/read_csv",
        Some(rmpv::Value::Map(vec![("path".into(), "t.csv".into())])),
        None,
    ))
    .unwrap_err();
    assert!(err.0.contains("scope is required"), "got: {}", err.0);
}

/// A call this connector does not answer says what it does answer.
#[test]
fn an_unknown_call_names_the_four() {
    let dir = tempfile::tempdir().unwrap();
    let err = call(
        &scope(dir.path()),
        "data/read_orc",
        vec![("path", rmpv::Value::from("t.orc"))],
    )
    .unwrap_err();
    assert!(err.0.contains("read_parquet"), "got: {}", err.0);
}

/// The connector declares a scope-type, so an unresolvable directory is a
/// startup refusal rather than a puzzling error on first call.
#[test]
fn a_scope_naming_no_directory_is_refused_at_startup() {
    let connector = DataConnector::new();
    let bad = Scope(rmpv::Value::from("/no/such/place/at/all"));
    assert!(connector.scope_type().validate(Some(&bad)).is_err());
    let dir = tempfile::tempdir().unwrap();
    assert!(connector
        .scope_type()
        .validate(Some(&scope(dir.path())))
        .is_ok());
}

/// `max_bytes` bounds both directions, and it is the memory bound too: a
/// parquet file is read whole.
#[test]
fn a_file_past_max_bytes_is_refused_in_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let tight = Scope(rmpv::Value::Map(vec![
        (
            "scope".into(),
            rmpv::Value::from(dir.path().to_str().unwrap()),
        ),
        ("access".into(), rmpv::Value::from("readwrite")),
        ("max_bytes".into(), rmpv::Value::from(64u64)),
    ]));
    let err = call(
        &tight,
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            (
                "columns",
                table(vec![(
                    "a",
                    f64_column(&(0..1000).map(|i| i as f64).collect::<Vec<_>>()),
                )]),
            ),
        ],
    )
    .unwrap_err();
    assert!(err.0.contains("max_bytes"), "got: {}", err.0);

    std::fs::write(dir.path().join("big.csv"), "a\n".repeat(100)).unwrap();
    let err = call(
        &tight,
        "data/read_csv",
        vec![("path", rmpv::Value::from("big.csv"))],
    )
    .unwrap_err();
    assert!(err.0.contains("max_bytes"), "got: {}", err.0);
}

/// Columns of different lengths are not a table, and saying so is better
/// than writing a file whose rows do not line up.
#[test]
fn columns_of_different_lengths_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let err = call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            (
                "columns",
                table(vec![
                    ("a", f64_column(&[1.0, 2.0])),
                    ("b", f64_column(&[1.0])),
                ]),
            ),
            ("order", rmpv::Value::Array(vec!["a".into(), "b".into()])),
        ],
    )
    .unwrap_err();
    assert!(err.0.contains("same length"), "got: {}", err.0);
}

/// A byte count that is not a whole number of elements is the guest's bug,
/// and it is caught before anything is written.
#[test]
fn a_partial_element_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let err = call(
        &rw(dir.path()),
        "data/write_parquet",
        vec![
            ("path", rmpv::Value::from("t.parquet")),
            (
                "columns",
                table(vec![(
                    "a",
                    rmpv::Value::Map(vec![
                        ("dtype".into(), rmpv::Value::from("f64")),
                        ("values".into(), rmpv::Value::Binary(vec![0; 12])),
                    ]),
                )]),
            ),
        ],
    )
    .unwrap_err();
    assert!(err.0.contains("eight-byte"), "got: {}", err.0);
}
