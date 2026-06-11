# Domain Docs

How engineering skills should consume this repo's domain documentation when exploring the codebase.

## Layout

This repo uses a single-context domain docs layout:

- `CONTEXT.md` at the repo root for domain language and project context.
- `docs/adr/` for architectural decision records.

These files may not exist yet. If absent, proceed silently. Do not suggest creating them upfront. Producer skills create them lazily when terms or decisions get resolved.

## Before exploring, read these

- `CONTEXT.md` at the repo root, if it exists.
- ADRs in `docs/adr/` that touch the area being changed, if any exist.

## Use glossary vocabulary

When output names a domain concept in an issue title, refactor proposal, hypothesis, or test name, use the term as defined in `CONTEXT.md`. Avoid synonyms the glossary explicitly rejects.

If needed concept is missing from the glossary, note it as a possible gap for `grill-with-docs`.

## Flag ADR conflicts

If output contradicts an existing ADR, surface it explicitly instead of silently overriding it.
