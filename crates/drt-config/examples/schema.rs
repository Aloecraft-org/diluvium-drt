//! The config file's JSON Schema, from the serde types that read it:
//! `cargo run -p drt-config --features schemars --example schema`.
//! `script/config-schema.sh` writes it to `doc/drt-config.schema.json` and
//! checks the copy there is current; the browser package ships it as
//! `config.schema.json` for a profile editor to validate against.

fn main() {
    let schema = schemars::schema_for!(drt_config::RootConfig);
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}
