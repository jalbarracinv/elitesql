# Changelog

## 0.1.0 — 2026-09-26

EliteSQL remains an alpha release. APIs and the on-disk format may still change.

- Faster primary-key page lookup and eligible SQL reads that defer decoding
  projected records until filtering, ordering and pagination select the result.
- Improved commit batching and arithmetic delta updates under contention, with
  predicate and constraint revalidation and protection for previously read rows.
- Fixed duplicate reachability and improved sidecar connection handling and CI
  test stability.
- Updated GitHub Actions to Node.js 24 runtimes and the Node client CI to Node.js 24.
- Replaced the benchmark report with a fresh EliteSQL-versus-SQLite comparison,
  including the mini-SaaS simulator, and preserved the previous report as
  `old_benchmark.md`. Measurements identify their source revision and configuration.
- Revised the README and repaired Python, Rust and CLI examples.
- Aligned Rust crates, the Python package and the Node client at version 0.1.0.

Python wheels include the native engine for Linux x86_64/aarch64
(manylinux_2_28) and macOS arm64. Python 3.9+ and glibc 2.28+ are required on
the applicable platforms. Source builds require Rust 1.89+.

See [benchmark.md](benchmark.md) for measured performance and limitations.
