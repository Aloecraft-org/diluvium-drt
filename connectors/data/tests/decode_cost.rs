//! A column of real size decodes, and what it costs is measured rather
//! than guessed.
//!
//! Two jobs. It is a gate — a million-row column has to round-trip
//! correctly, which the small fixtures in `columns.rs` cannot show — and it
//! is where `doc/Failure-Modes.md`'s FM-5 gets its number from, because
//! that entry is about work the guest's instruction budget does not bound
//! and an operator sizing `max_bytes` needs to know what a megabyte costs.
//!
//! Run with `--nocapture` to read the timings. They are printed, never
//! asserted on: a wall-clock assertion is a test that fails on a busy
//! machine for a reason that is not a defect.

use drt_caps::Scope;
use drt_connector::Connector;

fn rw(dir: &std::path::Path, max_bytes: u64) -> Scope {
    Scope(rmpv::Value::Map(vec![
        ("scope".into(), rmpv::Value::from(dir.to_str().unwrap())),
        ("access".into(), rmpv::Value::from("readwrite")),
        ("max_bytes".into(), rmpv::Value::from(max_bytes)),
    ]))
}

#[test]
fn a_million_row_column_round_trips_and_the_cost_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let c = drt_connector_data::DataConnector::new();
    let sc = rw(dir.path(), 64 * 1024 * 1024);

    for rows in [100_000usize, 1_000_000] {
        let values: Vec<f64> = (0..rows).map(|i| i as f64 * 1.5).collect();
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let args = rmpv::Value::Map(vec![
            ("path".into(), rmpv::Value::from("t.parquet")),
            (
                "columns".into(),
                rmpv::Value::Map(vec![(
                    "v".into(),
                    rmpv::Value::Map(vec![
                        ("dtype".into(), rmpv::Value::from("f64")),
                        ("values".into(), rmpv::Value::Binary(bytes)),
                    ]),
                )]),
            ),
        ]);
        pollster::block_on(c.call("data/write_parquet", Some(args), Some(&sc))).unwrap();
        let size = std::fs::metadata(dir.path().join("t.parquet"))
            .unwrap()
            .len();

        let read = rmpv::Value::Map(vec![("path".into(), rmpv::Value::from("t.parquet"))]);
        let start = std::time::Instant::now();
        let answer =
            pollster::block_on(c.call("data/read_parquet", Some(read), Some(&sc))).unwrap();
        let took = start.elapsed();
        let got = answer
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_str() == Some("rows"))
            .unwrap()
            .1
            .as_u64()
            .unwrap();
        assert_eq!(got as usize, rows);
        println!("{rows} rows, {size} bytes on disk, decoded in {took:?}");
    }
}
