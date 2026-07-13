# Unsafe safety audit baseline (2026-07-13)

`just unsafe-audit` is the blocking safety-documentation gate. It combines the
repository architecture scanner with Clippy's
`clippy::undocumented_unsafe_blocks` lint and a small check for `unsafe fn`
rustdoc contracts.

Current inventory:

- 313 `unsafe` occurrences under `src/`; 9 under `tests/`.
- 59 library unsafe blocks (72 including test targets) are currently rejected
  by Clippy because they lack an immediately preceding `// SAFETY:` comment.
- 8 unsafe functions lack a `# Safety` rustdoc section (`src/cpu.rs` and
  `src/io/uring.rs`).

This is intentionally a visible failing gate rather than an allowlist. Existing
violations must be documented in place; new `unsafe` code cannot be merged by
silently extending a baseline. The fast loop (`just test-fast`) does not invoke
this relatively slow Clippy gate yet. The submission gate (`just gate`) does.

Completion criteria:

1. `just unsafe-audit` exits zero.
2. Every unsafe block has a local `// SAFETY:` explanation describing the
   ownership, initialization, aliasing, lifetime, or syscall precondition.
3. Every `unsafe fn` has a `# Safety` section documenting caller obligations.
4. The explanation remains adjacent to the operation so code movement cannot
   leave an unrelated comment appearing to justify it.
