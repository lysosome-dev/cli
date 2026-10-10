//! lysosome-core: finds the disk your dev tools left behind. Returns data; never prints.

/// Product name. Change it here and everything else follows.
pub const TOOL_NAME: &str = "lysosome";

/// Version of the JSON output contract (see `schema/`).
pub const SCHEMA_VERSION: u32 = 1;

pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
