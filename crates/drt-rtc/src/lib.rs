//! The WebRTC host: a browser reaches this process directly over a data
//! channel, and the host carries Wisp v1 streams to a scoped set of TCP
//! targets. `doc/BrowserAccess.md` is the wire, normatively;
//! `doc/Plan-0.8.0.md` §3 is why it is shaped this way.
//!
//! Nothing here knows how the two sides found each other. The program does
//! that, over whatever signaling it has, and hands the host a browser's
//! presence record; the host hands back its own. That keeps Discofetch's API
//! out of the binary, as WireGuard's rendezvous is.
//!
//! ## surface block
//!
//! - Entry points: [`Host::start`] and its [`Command`]s and [`Event`]s;
//!   [`Record::decode`] / [`Record::encode`] / [`record::answer_sdp`];
//!   [`wisp::parse`] and the packet builders; [`Entry::parse`] and
//!   [`Scope::allows`]; [`Identity::load_or_create`].
//! - Configurable: [`HostConfig`], and the constants each module lists in
//!   its own surface block.
//! - Fan-out: the modules.
//!   - [`record`]: the presence record and the answer a browser builds.
//!   - [`wisp`]: Wisp v1 packets, bytes only.
//!   - [`scope`]: what a `CONNECT` may reach, before and after resolution.
//!   - [`identity`]: the credentials and certificate kept across restarts.
//!   - [`host`]: the socket, the sessions, the Wisp server, the splice.

pub mod host;
pub mod identity;
pub mod record;
pub mod scope;
pub mod wisp;

pub use host::{Command, Event, Host, HostConfig, SessionState, StreamState};
pub use identity::Identity;
pub use record::Record;
pub use scope::{Entry, Scope};
