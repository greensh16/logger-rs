//! HPC job telemetry collection.
//!
//! All logic lives in the library so that integration tests — in particular
//! `tests/schema_contract.rs`, which pins the NDJSON format the dashboard
//! parses — can exercise the same code the binary runs.

pub mod cgroup;
pub mod check;
pub mod cli;
pub mod cpu;
pub mod gpu;
pub mod host;
pub mod logger;
pub mod manifest;
pub mod merge;
pub mod network;
pub mod output;
pub mod process;
pub mod scheduler;
pub mod types;
