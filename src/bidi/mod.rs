//! WebDriver BiDi transport and a thin typed wrapper over the commands the CLI
//! needs. BiDi is Firefox's bidirectional debugging protocol: JSON messages with
//! an `id`/`method`/`params` request shape and `type: success|error|event`
//! responses — structurally very close to Chrome's CDP.

pub mod client;
pub mod session;

pub use client::BidiClient;
pub use session::BidiSession;
