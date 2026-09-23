//! The extraction core, shared by the `quartz-ctx` server and by cortex.
//!
//! cortex links this rather than running an extractor of its own, so the two
//! servers cannot disagree about what a type is: one parser, one resolution
//! pass, one incremental cache.
#![allow(dead_code, unused_imports, unused_variables)]

pub mod bridge;
pub mod calls;
pub mod incremental;
pub mod lang;
pub mod model;
pub mod parser;
