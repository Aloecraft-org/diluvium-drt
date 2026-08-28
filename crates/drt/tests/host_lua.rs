//! The `.host.lua` loader: a diluvium-host deployment moves to DRT by
//! swapping the binary and changing no files. The fixtures here mirror the
//! discofetch deployments' shapes (api.host.lua, cap6.host.lua), which are
//! also smoke-tested against the real thing outside CI.

use drt::config;

fn write_deployment(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    dir
}

/// The api.host.lua shape: supervisor beside the config, bare-string caps,
/// a listen block with the C's field names (`port`, `bind`, `deadline_ms`).
#[test]
fn the_c_hosts_dialect_maps_field_for_field() {
    let dir = write_deployment(&[
        (
            "api.host.lua",
            r#"
            -- comments survive, naturally
            return {
              supervisor = "supervisor.lua",
              caps = { "queue:*", "host:fs/*" },
              connectors = {
                listen = {
                  port = 8080,
                  bind = "0.0.0.0",
                  queue = "http_in",
                  reply_queue = "http_out",
                  max_body = 65536,
                  deadline_ms = 5000,
                  max_conns = 64,
                  headers = { "authorization", "x-df-sub", "host" },
                  response_headers = { "location", "retry-after" },
                },
                fs = { scope = ".work", access = "readwrite", max_bytes = 65536 },
                time = true,
              },
            }
            "#,
        ),
        ("supervisor.lua", "local x = 1\n"),
    ]);
    let cfg = config::load(Some(&dir.path().join("api.host.lua"))).unwrap();

    // The supervisor resolves beside the config file: the deployment
    // directory is the unit that moves.
    assert_eq!(
        cfg.root.program,
        Some(drt_config::Program::Path(dir.path().join("supervisor.lua")))
    );
    assert_eq!(cfg.root.caps.len(), 2);
    assert_eq!(cfg.root.caps[0].capability, "queue:*");

    let listener = &cfg.listeners[0];
    assert_eq!(listener.scheme, "http");
    assert_eq!(listener.address, "0.0.0.0:8080");
    assert_eq!(listener.conn_deadline_ms, 5000);
    assert_eq!(listener.headers, vec!["authorization", "x-df-sub", "host"]);
    // `response_headers` is the C host's config spelling for what the
    // struct calls resp_headers.
    assert_eq!(listener.resp_headers, vec!["location", "retry-after"]);

    // A connector block is the scope; an empty block wires with none.
    let fs = &cfg.connectors["fs"];
    assert!(fs.scope.is_some());
    // `time = true` wires the connector with no scope of its own.
    assert!(cfg.connectors["time"].scope.is_none());
}

/// cap6's shape end to end: the deployment runs under `drt start` from its
/// `.host.lua`, the guest works through `host.fs`, and the evidence lands
/// on disk — the whole conversion story in one test.
#[test]
fn a_cap6_shaped_deployment_runs_from_its_host_lua() {
    let dir = write_deployment(&[
        (
            "cap.host.lua",
            r#"
            return {
              supervisor = "supervisor.lua",
              caps = { "host:fs/*" },
              connectors = {
                fs = { scope = "SCOPE", access = "readwrite", max_bytes = 65536 },
              },
            }
            "#,
        ),
        (
            "supervisor.lua",
            "host.fs.write('note.txt', 'moved hosts, changed nothing')\n\
             assert(host.fs.read('note.txt') == 'moved hosts, changed nothing')\n",
        ),
    ]);
    // The scope must be absolute for the test: the fixture's cwd is not the
    // deployment dir, unlike a real `drt start` run.
    let scope_dir = dir.path().join("work");
    std::fs::create_dir(&scope_dir).unwrap();
    let config_path = dir.path().join("cap.host.lua");
    let text = std::fs::read_to_string(&config_path)
        .unwrap()
        .replace("SCOPE", scope_dir.to_str().unwrap());
    std::fs::write(&config_path, text).unwrap();

    let cfg = config::load(Some(&config_path)).unwrap();
    let mut registry = drt_connector::Registry::new();
    registry
        .wire(
            "fs",
            std::sync::Arc::new(drt_connector_fs::FsConnector::new()),
            cfg.connectors["fs"].scope.clone(),
        )
        .unwrap();
    drt::start::start(&cfg, drt_connector::Dispatcher::new(registry)).unwrap();
    assert_eq!(
        std::fs::read_to_string(scope_dir.join("note.txt")).unwrap(),
        "moved hosts, changed nothing"
    );
}

/// The C loader's promise, kept: an unknown key is an error and names
/// itself — a typo about to become a silent default is the failure mode.
#[test]
fn an_unknown_key_is_an_error_that_names_itself() {
    let dir = write_deployment(&[(
        "typo.host.lua",
        r#"return { supervizor = "supervisor.lua" }"#,
    )]);
    let err = config::load(Some(&dir.path().join("typo.host.lua"))).unwrap_err();
    assert!(err.contains("'supervizor'"), "{err}");
    assert!(err.contains("known:"), "{err}");
}

/// Not data all the way down — a function in the table — is refused by the
/// engine itself, and a config that loops runs out of its budget instead of
/// hanging the process.
#[test]
fn a_config_that_is_not_data_is_refused() {
    let dir = write_deployment(&[("fn.host.lua", r#"return { supervisor = function() end }"#)]);
    assert!(config::load(Some(&dir.path().join("fn.host.lua"))).is_err());

    let dir = write_deployment(&[("loop.host.lua", "while true do end\nreturn {}")]);
    let err = config::load(Some(&dir.path().join("loop.host.lua"))).unwrap_err();
    assert!(!err.is_empty());
}

/// The `relay` block: DRT's own extension to the dialect, because a
/// rendezvous fetchpoint is configured like everything else on the box.
/// `bind` may carry the port or pair with one, the way `listen` does.
#[cfg(feature = "relay")]
#[test]
fn a_rendezvous_fetchpoint_configures_its_relay_in_host_lua() {
    let dir = write_deployment(&[
        (
            "rv.host.lua",
            r#"
            return {
              supervisor = "supervisor.lua",
              relay = {
                bind = "0.0.0.0",
                port = 8443,
                queue = "relay_in",
                reply_queue = "relay_out",
                admit_timeout_ms = 1500,
                labels = {
                  xps = { park_key = "pk-0123456789abcdef",
                          caller_key = "ck-0123456789abcdef" },
                },
              },
            }
            "#,
        ),
        ("supervisor.lua", "local x = 1\n"),
    ]);
    let relay = config::load(Some(&dir.path().join("rv.host.lua")))
        .unwrap()
        .relay
        .expect("the relay block should load");
    assert_eq!(relay.bind, "0.0.0.0:8443");
    assert_eq!(relay.queue, "relay_in");
    assert_eq!(relay.reply_queue, "relay_out");
    assert_eq!(relay.admit_timeout_ms, 1500);
    assert_eq!(relay.labels["xps"].park_key, "pk-0123456789abcdef");

    // host:port in one string, and no reply queue: the default posture,
    // where the static key is the only gate.
    let dir = write_deployment(&[(
        "rv.host.lua",
        r#"return { relay = { bind = "0.0.0.0:8443",
                             labels = { xps = { park_key = "p", caller_key = "c" } } } }"#,
    )]);
    let relay = config::load(Some(&dir.path().join("rv.host.lua")))
        .unwrap()
        .relay
        .unwrap();
    assert_eq!(relay.bind, "0.0.0.0:8443");
    assert!(
        relay.reply_queue.is_empty(),
        "arbitration is opt-in: {relay:?}"
    );
}

/// The refusals that keep a relay from being an open door: a label missing
/// one of its two keys, and a bind with nowhere to bind.
#[cfg(feature = "relay")]
#[test]
fn a_relay_block_refuses_a_half_configured_label() {
    let dir = write_deployment(&[(
        "rv.host.lua",
        r#"return { relay = { bind = "0.0.0.0:8443",
                             labels = { xps = { park_key = "p" } } } }"#,
    )]);
    let err = config::load(Some(&dir.path().join("rv.host.lua"))).unwrap_err();
    assert!(err.contains("caller_key"), "{err}");

    let dir = write_deployment(&[(
        "rv.host.lua",
        r#"return { relay = { bind = "0.0.0.0",
                             labels = { xps = { park_key = "p", caller_key = "c" } } } }"#,
    )]);
    let err = config::load(Some(&dir.path().join("rv.host.lua"))).unwrap_err();
    assert!(err.contains("no port"), "{err}");
}
