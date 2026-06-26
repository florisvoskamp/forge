//! Language-server-backed live diagnostics for Forge.
//!
//! Forge can surface the *same* errors a developer's editor sees by talking to real language servers
//! (rust-analyzer, pyright, tsserver, …) over LSP. This crate is the integration:
//!
//! - [`registry`] — maps a file to its configured language server (and resolves the binary on PATH);
//! - [`rpc`] — the JSON-RPC framing over the server's stdio;
//! - [`server`] — a per-language server process Forge spawns, initializes, and queries;
//! - [`types`] — the small, transport-agnostic [`Diagnostic`] / [`DiagnosticSeverity`] surface the
//!   rest of Forge consumes (so callers never touch raw LSP JSON).
//!
//! The entry point is [`LspRegistry`]. Everything is best-effort — a missing server or an
//! unsupported language yields no diagnostics rather than an error, so live diagnostics never block
//! a turn.

pub mod registry;
pub mod rpc;
pub mod server;
pub mod types;

pub use registry::LspRegistry;
pub use types::{Diagnostic, DiagnosticSeverity};
