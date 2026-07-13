# Unsafe safety audit baseline (2026-07-13)

`just unsafe-audit` is the blocking safety-documentation gate. It combines the
repository architecture scanner with Clippy's
`clippy::undocumented_unsafe_blocks` lint and a small check for `unsafe fn`
rustdoc contracts.

Initial inventory before remediation:

- 313 `unsafe` occurrences under `src/`; 9 under `tests/`.
- 59 library unsafe blocks (72 including test targets) were rejected by Clippy
  because they lacked an immediately preceding `// SAFETY:` comment.
- 8 unsafe functions lacked a `# Safety` rustdoc section (`src/cpu.rs` and
  `src/io/uring.rs`).

The initial inventory was remediated in commit `4245be1`. The current gate runs
clean: all 294 unsafe blocks have a local explanation and all unsafe functions
have a `# Safety` section.

This is intentionally a blocking gate rather than an allowlist. New `unsafe`
code cannot be merged without an explanation. The fast loop (`just test-fast`)
does not invoke this relatively slow Clippy gate; the submission gate (`just
gate`) does.

Completion criteria:

1. `just unsafe-audit` exits zero.
2. Every unsafe block has a local `// SAFETY:` explanation describing the
   ownership, initialization, aliasing, lifetime, or syscall precondition.
3. Every `unsafe fn` has a `# Safety` section documenting caller obligations.
4. The explanation remains adjacent to the operation so code movement cannot
   leave an unrelated comment appearing to justify it.
