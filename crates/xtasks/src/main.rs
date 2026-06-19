//! Forge developer tasks. Currently: the Lattice token-savings benchmark.
//!
//! Run: `cargo run -p xtasks -- bench-lattice`

mod bench_lattice;
mod probe_retrieve;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "bench-lattice" => bench_lattice::run().await,
        "probe-retrieve" => probe_retrieve::run(),
        other => anyhow::bail!("unknown subcommand: {other:?} (try: bench-lattice, probe-retrieve)"),
    }
}
