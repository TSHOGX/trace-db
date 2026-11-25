# Security Audit Exceptions

CI runs `cargo audit --deny warnings` for the locked dependency graph. The
explicit exceptions are `RUSTSEC-2025-0057` for `fxhash 0.2.1`, an unmaintained
(not vulnerable) transitive dependency of the pinned `jieba-rs 0.7.0` tokenizer,
and `RUSTSEC-2026-0543` for `chrono 0.4.45`, which is also unmaintained rather
than vulnerable.

The exceptions are narrowly scoped to those advisory IDs. They do not suppress
security vulnerabilities, unsoundness, or yanked-package findings. Revisit the
`fxhash` exception when `jieba-rs` offers a compatible release without it, and
the `chrono` exception if the project migrates to another date-time crate.
