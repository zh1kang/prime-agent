//! Private-frame codec re-export. The codec is the shared worker-socket wire
//! contract (served by this crate's workers, spoken by direct-attach clients
//! in radiata-tui/radiata-cli), so it lives in [`radiata_types::daemon::framing`].

pub use radiata_types::daemon::framing::*;
