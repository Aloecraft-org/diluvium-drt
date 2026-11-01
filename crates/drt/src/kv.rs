//! `drt kv`: the kv store a signalling server keeps (`drt p2p --match
//! --kv`, `doc/DRT-Signalling.md` §10), from the command line.
//!
//! ## surface block
//!
//! - Entry points: [`Args`] and [`Action`], the command line; [`run`],
//!   one action carried out, returning the exit status.
//! - Configurable: [`MAX_LIST_REPLY`], the largest listing read.
//! - Fan-out: [`Action`], the five actions, and the one match on it in
//!   [`run`]. Every action is one request of DRT-Signalling §10, made through
//!   [`crate::p2p::http`]; `watch` reads the event stream as it arrives.
//!
//! Exit status: 0 done, 1 refused (no such key, a lost compare-and-set,
//! the server's refusal), 2 a request that never reached an answer.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use tokio_rustls::rustls::pki_types::CertificateDer;

use crate::p2p::http;

/// A listing is at most `KV_LIST_MAX` (1000) values of 16 KiB.
pub const MAX_LIST_REPLY: usize = 17 * 1024 * 1024;

/// `drt kv`'s command line.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
    /// A header for the server, name=value; auth=<token> is shorthand for
    /// Authorization: Bearer <token>. Repeatable.
    #[arg(long = "H", value_name = "NAME=VALUE", global = true)]
    pub header: Vec<String>,
    /// Trust this PEM certificate beside the public roots. Repeatable.
    #[arg(long = "extra-root", value_name = "PEM", global = true)]
    pub extra_root: Vec<PathBuf>,
}

/// Each action takes the key's URL: drt://host[:port]/v1/<topic>/kv/<key>
/// (https unless the host is loopback, as `drt p2p` reads it), or the same
/// as http(s)://.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum Action {
    /// Print a key's value. Exits 1 when the key is absent.
    Get {
        #[arg(value_name = "URL")]
        url: String,
        /// Print {"key", "value", "version"} as JSON instead.
        #[arg(long)]
        json: bool,
    },
    /// Write a key: VALUE, or stdin when VALUE is - or absent. Prints the
    /// new version.
    Put {
        #[arg(value_name = "URL")]
        url: String,
        #[arg(value_name = "VALUE")]
        value: Option<String>,
        /// Expire the key after this many seconds.
        #[arg(long, value_name = "SECONDS")]
        ttl: Option<u64>,
        /// Write only over this version (compare-and-set); 0 writes only
        /// where the key is absent. A lost race exits 1.
        #[arg(long = "if-version", value_name = "N")]
        if_version: Option<u64>,
    },
    /// Delete a key. Exits 1 when it was absent.
    Rm {
        #[arg(value_name = "URL")]
        url: String,
        /// Delete only this version (compare-and-set).
        #[arg(long = "if-version", value_name = "N")]
        if_version: Option<u64>,
    },
    /// List the keys under a prefix: .../kv for all of a topic,
    /// .../kv/<prefix> for those starting with it. One key per line, with
    /// its version.
    Ls {
        #[arg(value_name = "URL")]
        url: String,
        /// At most this many keys (the server's page: 1 to 1000).
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
        /// Print the server's JSON page instead.
        #[arg(long)]
        json: bool,
    },
    /// Print each change under a prefix as one JSON line, as it happens,
    /// until interrupted. --since resumes after a revision.
    Watch {
        #[arg(value_name = "URL")]
        url: String,
        /// Replay the changes after this revision first, as far back as the
        /// server keeps (a reset line says when that is not far enough).
        #[arg(long, value_name = "REVISION")]
        since: Option<u64>,
    },
}

/// A key's URL, split: the topic's base and the key (or prefix) after
/// `/kv/`, still escaped as typed.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    /// `http(s)://host[:port]/v1/<topic>/kv`
    kv: String,
    key: String,
}

impl Target {
    fn parse(url: &str) -> Result<Target, String> {
        let http = match url.split_once("://") {
            Some(("drt", rest)) => {
                let authority = rest.split('/').next().unwrap_or("");
                let host = match authority.rsplit_once(':') {
                    Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) && !h.is_empty() => h,
                    _ => authority,
                };
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let loopback = host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback());
                format!("{}://{rest}", if loopback { "http" } else { "https" })
            }
            Some(("http" | "https", _)) => url.to_string(),
            _ => return Err(format!("'{url}' is not drt://, http:// or https://")),
        };
        let (scheme, rest) = http.split_once("://").expect("made above");
        let Some((authority, path)) = rest.split_once('/') else {
            return Err(format!("'{url}' names no /v1/<topic>/kv"));
        };
        let mut parts = path.splitn(4, '/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("v1"), Some(topic), Some("kv")) if !topic.is_empty() => Ok(Target {
                kv: format!("{scheme}://{authority}/v1/{topic}/kv"),
                key: parts.next().unwrap_or("").to_string(),
            }),
            _ => Err(format!(
                "'{url}' is not .../v1/<topic>/kv/<key>; the topic and `kv` come first"
            )),
        }
    }

    fn key_url(&self) -> Result<String, String> {
        if self.key.is_empty() {
            return Err(format!("'{}' names no key after /kv/", self.kv));
        }
        Ok(format!("{}/{}", self.kv, self.key))
    }
}

/// One action, and the status to exit with.
pub fn run(args: &Args) -> u8 {
    let outcome = (|| {
        let headers = crate::p2p::headers(&BTreeMap::new(), &args.header, &crate::prompt::terminal)?;
        let roots = crate::roots::load_roots_named("--extra-root", &args.extra_root)?;
        let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("a runtime: {e}"))?;
        let outcome = runtime.block_on(act(&args.action, &headers, &roots));
        // Leaked for the reason every networked verb leaks one: tokio
        // 1.53.1's teardown race (cli.rs's note beside `drt tunnel`).
        std::mem::forget(runtime);
        outcome
    })();
    match outcome {
        Ok(status) => status,
        Err(e) => {
            eprintln!("drt kv: {e}");
            2
        }
    }
}

// depth: the actions, one request each

/// The server's refusal in its own words, and exit 1.
fn refused(resp: &http::Response, url: &str) -> Result<u8, String> {
    eprintln!(
        "drt kv: {}",
        resp.refusal(url)
            .unwrap_or_else(|| format!("{url} answered {}", resp.status))
    );
    Ok(1)
}

fn version_of(resp: &http::Response) -> Option<u64> {
    resp.header("etag")?
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .ok()
}

fn with(headers: &[(String, String)], extra: &[(&str, String)]) -> Vec<(String, String)> {
    let mut out = headers.to_vec();
    out.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
    out
}

async fn act(
    action: &Action,
    headers: &[(String, String)],
    roots: &[CertificateDer<'static>],
) -> Result<u8, String> {
    match action {
        Action::Get { url, json } => {
            let target = Target::parse(url)?;
            let at = target.key_url()?;
            let resp = http::request("GET", &at, headers, None, roots).await?;
            match resp.status {
                200 if *json => {
                    let out = serde_json::json!({
                        "key": target.key,
                        "value": resp.text(),
                        "version": version_of(&resp),
                    });
                    println!("{out}");
                    Ok(0)
                }
                200 => {
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(&resp.body);
                    // A value is printed as it is, so `> file` keeps it
                    // byte for byte; a terminal gets the line ended.
                    if stdout.is_terminal() && !resp.body.ends_with(b"\n") {
                        let _ = stdout.write_all(b"\n");
                    }
                    Ok(0)
                }
                _ => refused(&resp, &at),
            }
        }
        Action::Put {
            url,
            value,
            ttl,
            if_version,
        } => {
            let target = Target::parse(url)?;
            let mut at = target.key_url()?;
            let value = match value.as_deref() {
                Some("-") | None => {
                    let mut text = String::new();
                    std::io::stdin()
                        .read_to_string(&mut text)
                        .map_err(|e| format!("stdin: {e}"))?;
                    text
                }
                Some(v) => v.to_string(),
            };
            if let Some(ttl) = ttl {
                at.push_str(&format!("?ttl={ttl}"));
            }
            let extra = match if_version {
                Some(0) => vec![("If-None-Match", "*".to_string())],
                Some(v) => vec![("If-Match", format!("\"{v}\""))],
                None => vec![],
            };
            let resp =
                http::request("PUT", &at, &with(headers, &extra), Some(&value), roots).await?;
            match resp.status {
                200 | 201 => {
                    println!(
                        "{}",
                        version_of(&resp).map_or("?".into(), |v| v.to_string())
                    );
                    Ok(0)
                }
                412 => {
                    match version_of(&resp) {
                        Some(v) => eprintln!("drt kv: the key is at version {v}, not that one"),
                        None if *if_version == Some(0) => eprintln!("drt kv: the key exists"),
                        None => eprintln!("drt kv: the key is absent"),
                    }
                    Ok(1)
                }
                _ => refused(&resp, &at),
            }
        }
        Action::Rm { url, if_version } => {
            let target = Target::parse(url)?;
            let at = target.key_url()?;
            let extra = match if_version {
                Some(v) => vec![("If-Match", format!("\"{v}\""))],
                None => vec![],
            };
            let resp = http::request("DELETE", &at, &with(headers, &extra), None, roots).await?;
            match resp.status {
                204 => Ok(0),
                412 => {
                    match version_of(&resp) {
                        Some(v) => eprintln!("drt kv: the key is at version {v}, not that one"),
                        None => eprintln!("drt kv: the key is absent"),
                    }
                    Ok(1)
                }
                _ => refused(&resp, &at),
            }
        }
        Action::Ls { url, limit, json } => {
            let target = Target::parse(url)?;
            let mut at = format!("{}?prefix={}", target.kv, target.key);
            if let Some(n) = limit {
                at.push_str(&format!("&limit={n}"));
            }
            let resp =
                http::request_limited("GET", &at, headers, None, roots, MAX_LIST_REPLY).await?;
            if resp.status != 200 {
                return refused(&resp, &at);
            }
            if *json {
                println!("{}", resp.text());
                return Ok(0);
            }
            let page: serde_json::Value = serde_json::from_slice(&resp.body)
                .map_err(|e| format!("{at} answered a listing that is not JSON: {e}"))?;
            for k in page["keys"].as_array().into_iter().flatten() {
                let expires = k["expires_in"]
                    .as_u64()
                    .map(|s| format!(", expires in {s}s"))
                    .unwrap_or_default();
                println!(
                    "{}\tv{}{expires}",
                    k["key"].as_str().unwrap_or(""),
                    k["version"]
                );
            }
            if page["more"].as_bool() == Some(true) {
                eprintln!("drt kv: more keys follow; --limit or a longer prefix");
            }
            Ok(0)
        }
        Action::Watch { url, since } => {
            let target = Target::parse(url)?;
            let mut at = format!("{}?watch=stream&prefix={}", target.kv, target.key);
            if let Some(r) = since {
                at.push_str(&format!("&since={r}"));
            }
            let (head, mut body) = http::stream("GET", &at, headers, None, roots).await?;
            if head.status != 200 {
                let mut rest = Vec::new();
                while let Ok(Some(chunk)) = body.next().await {
                    rest.extend_from_slice(&chunk);
                }
                let resp = http::Response { body: rest, ..head };
                return refused(&resp, &at);
            }
            // Server-sent events, one `data:` line each; a comment line is
            // the keepalive and prints nothing.
            let mut pending = String::new();
            while let Some(chunk) = body.next().await? {
                pending.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = pending.find("\n\n") {
                    let event: String = pending.drain(..end + 2).collect();
                    let kind = event
                        .lines()
                        .find_map(|l| l.strip_prefix("event: "))
                        .unwrap_or("");
                    if let Some(data) = event.lines().find_map(|l| l.strip_prefix("data: ")) {
                        match kind {
                            "reset" => {
                                eprintln!("drt kv: the server no longer keeps changes that far back; list again");
                                println!("{data}");
                            }
                            _ => println!("{data}"),
                        }
                        let _ = std::io::stdout().flush();
                    }
                }
            }
            eprintln!("drt kv: the server closed the stream");
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_names_a_topic_and_a_key() {
        let t = Target::parse("drt://127.0.0.1:8090/v1/home/kv/bpi/info").unwrap();
        assert_eq!(t.kv, "http://127.0.0.1:8090/v1/home/kv");
        assert_eq!(t.key, "bpi/info");
        let t = Target::parse("drt://match.example/v1/home/kv").unwrap();
        assert_eq!(t.kv, "https://match.example/v1/home/kv");
        assert_eq!(t.key, "");
        assert!(t.key_url().is_err());
        let t = Target::parse("http://h:1/v1/t/kv/").unwrap();
        assert_eq!(t.key, "");
        assert!(Target::parse("drt://h/v1/home/calls").is_err());
        assert!(Target::parse("drt://h/v1//kv/x").is_err());
        assert!(Target::parse("ftp://h/v1/t/kv/x").is_err());
    }
}
