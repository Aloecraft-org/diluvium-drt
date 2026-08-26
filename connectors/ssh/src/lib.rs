//! The ssh client connector: `host:ssh/exec` (SPEC.md §7).
//!
//! The scope is the place, per the capability model: *which host, as which
//! user, with which key* is the host's wiring; the program names only the
//! command it wants run there. Trust is explicit — the scope must carry the
//! host's public key or fingerprint, because trust-on-first-use is a
//! caller's decision and never a default (ego-transport enforces the same
//! posture underneath: pubkey-only, modern suite only, russh, never
//! hand-rolled).
//!
//! `exec`'s caveats apply verbatim (Capabilities.md §5, GUARANTEES.md): the
//! command runs on another machine entirely outside every sandbox here, the
//! instruction budget cannot bound it, and what bounds it instead is a
//! wall-clock timeout and an output cap — both in the scope, both host-side.
//! Granting `host:ssh/exec` is leaving the sandbox twice over. The reply is
//! still just a message: a replay replays the logged reply and never re-runs
//! the command.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};
use ego_transport::ssh::{
    private_key_from_openssh, public_key_from_openssh, HostKeyVerification, SshChannelEvent,
    SshClientConfig, SshClientConnection,
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

/// The scope-type `host:ssh/*` declares: where, as whom, with what, trusting
/// which host key. Validated at startup, by name — including that the key
/// file exists and parses, so a bad path is a named refusal at boot rather
/// than an auth failure at 3am.
#[derive(Debug, Clone, Deserialize)]
struct SshScope {
    /// `host:port` to dial.
    host: String,
    user: String,
    /// OpenSSH-format private key file (the client identity). A path, not
    /// key material: config wires places, and key bytes do not belong in it.
    key_path: PathBuf,
    /// The host's public key, OpenSSH one-line format. One of `host_key` /
    /// `host_fingerprint` is required.
    #[serde(default)]
    host_key: Option<String>,
    /// The host key's `SHA256:...` fingerprint.
    #[serde(default)]
    host_fingerprint: Option<String>,
    /// Wall-clock bound on one `ssh/exec` call, connection included. The
    /// only time bound there is — the instruction budget cannot reach a
    /// remote process.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Cap on collected stdout+stderr; past it the call errors rather than
    /// allocating without bound.
    #[serde(default)]
    max_output_bytes: Option<u64>,
}

impl SshScope {
    fn parse(scope: Option<&Scope>) -> Result<Self, String> {
        let Some(Scope(value)) = scope else {
            return Err("scope is required".into());
        };
        let scope: SshScope = rmpv::ext::from_value(value.clone())
            .map_err(|e| format!("scope does not parse: {e}"))?;
        if scope.host.is_empty() {
            return Err("scope.host is empty".into());
        }
        if scope.user.is_empty() {
            return Err("scope.user is empty".into());
        }
        if scope.host_key.is_none() && scope.host_fingerprint.is_none() {
            return Err(
                "scope names no trust anchor: set host_key (OpenSSH public key) or \
                 host_fingerprint (SHA256:...); trust-on-first-use is never the default"
                    .into(),
            );
        }
        if scope.timeout_ms == Some(0) {
            return Err("scope.timeout_ms of 0 would refuse every call".into());
        }
        Ok(scope)
    }

    fn verification(&self) -> Result<HostKeyVerification, String> {
        if let Some(line) = &self.host_key {
            let key = public_key_from_openssh(line).map_err(|e| e.to_string())?;
            return Ok(HostKeyVerification::Keys(vec![key]));
        }
        Ok(HostKeyVerification::Fingerprints(vec![self
            .host_fingerprint
            .clone()
            .expect("checked in parse")]))
    }

    fn client_config(&self) -> Result<SshClientConfig, String> {
        let pem = std::fs::read_to_string(&self.key_path)
            .map_err(|e| format!("cannot read key file {}: {e}", self.key_path.display()))?;
        let key = private_key_from_openssh(&pem, None).map_err(|e| e.to_string())?;
        Ok(SshClientConfig {
            user: self.user.clone(),
            key,
            host_verification: self.verification()?,
            inactivity_timeout: Some(Duration::from_millis(
                self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            )),
        })
    }
}

struct SshScopeType;

impl ScopeType for SshScopeType {
    fn describe(&self) -> &str {
        "{host, user, key_path, host_key|host_fingerprint, timeout_ms?, max_output_bytes?}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        let parsed = SshScope::parse(scope)?;
        // Fail at startup, by name: the key must exist and parse, and a
        // stated host_key must be a real public key.
        parsed.client_config().map(|_| ())
    }
}

#[derive(Debug, Deserialize)]
struct ExecArgs {
    command: String,
}

/// What came back, encoded as the reply value: `{exit, stdout, stderr}`.
/// `exit` is absent when the channel closed without reporting one — worded
/// as absence rather than a fake zero.
fn exec_value(exit: Option<u32>, stdout: Vec<u8>, stderr: Vec<u8>) -> rmpv::Value {
    let mut map = vec![
        ("stdout".into(), rmpv::Value::Binary(stdout)),
        ("stderr".into(), rmpv::Value::Binary(stderr)),
    ];
    if let Some(code) = exit {
        map.insert(0, ("exit".into(), rmpv::Value::from(code)));
    }
    rmpv::Value::Map(map)
}

/// One connection per call in v1; pooling is a seam, not a promise — a
/// connector restart (or none existing yet) is invisible to guests either
/// way, which is the property that matters.
#[derive(Default)]
pub struct SshConnector;

impl SshConnector {
    pub fn new() -> Self {
        SshConnector
    }
}

#[async_trait::async_trait]
impl Connector for SshConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(SshScopeType)
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        if call != "ssh/exec" {
            return Err(CallError::new(format!(
                "the ssh connector answers 'ssh/exec'; '{call}' is not it"
            )));
        }
        let scope = SshScope::parse(scope).map_err(CallError::new)?;
        let args: ExecArgs = rmpv::ext::from_value(
            args.ok_or_else(|| CallError::new("ssh/exec takes args {command}"))?,
        )
        .map_err(|e| CallError::new(format!("ssh/exec args do not parse: {e}")))?;

        let timeout = Duration::from_millis(scope.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let cap = scope.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES) as usize;
        let config = scope.client_config().map_err(CallError::new)?;

        tokio::time::timeout(timeout, exec_once(&scope.host, config, &args.command, cap))
            .await
            .unwrap_or_else(|_| {
                Err(CallError::new(format!(
                    "ssh/exec timed out after {}ms (the wall clock is the only bound a remote \
                     command has)",
                    timeout.as_millis()
                )))
            })
    }
}

async fn exec_once(host: &str, config: SshClientConfig, command: &str, cap: usize) -> CallResult {
    let conn = SshClientConnection::connect(host, config)
        .await
        .map_err(|e| CallError::new(format!("connecting to {host}: {e}")))?;
    let mut channel = conn
        .open_exec(command.as_bytes())
        .await
        .map_err(|e| CallError::new(format!("opening exec channel: {e}")))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit = None;
    loop {
        match channel.next_event().await {
            SshChannelEvent::Data(bytes) => stdout.extend_from_slice(&bytes),
            SshChannelEvent::ExtendedData(bytes) => stderr.extend_from_slice(&bytes),
            SshChannelEvent::ExitStatus(code) => exit = Some(code),
            SshChannelEvent::Eof => {}
            SshChannelEvent::Closed => break,
            SshChannelEvent::WindowChange { .. } => {}
        }
        if stdout.len() + stderr.len() > cap {
            let _ = conn.disconnect().await;
            return Err(CallError::new(format!(
                "ssh/exec output exceeded the {cap}-byte cap; raise max_output_bytes in the \
                 scope if this command's output is meant to be this large"
            )));
        }
    }
    let _ = conn.disconnect().await;
    Ok(exec_value(exit, stdout, stderr))
}
