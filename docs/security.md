# Security Audit Exceptions

CI runs `cargo audit --deny warnings` for the locked dependency graph. The
single explicit exception is `RUSTSEC-2025-0057` for `fxhash 0.2.1`, which is an
unmaintained (not vulnerable) transitive dependency of the pinned
`jieba-rs 0.7.0` tokenizer.

The exception is narrowly scoped to that advisory ID. It does not suppress
security vulnerabilities, unsoundness, or yanked-package findings. Revisit it
when `jieba-rs` offers a Rust 1.83-compatible release without `fxhash`.
