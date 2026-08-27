//! The DRT runtime, as a library. The `drt` binary is a thin CLI over
//! this; keeping the flow here is what lets it be tested end to end.

pub mod config;
pub mod run;
pub mod start;
