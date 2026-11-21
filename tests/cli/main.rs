//! End-to-end tests: each one runs the `tfex` binary against directories and
//! Git repositories created in a temporary workspace.

mod fetch;
mod graph;
mod incremental;
mod index_location;
mod plan;
mod query;
mod scan;
mod state_mapping;
mod support;
