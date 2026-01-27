---
name: rust
description: Rust development guidelines and workflow
---

# Rust Development

## Commands

- `cargo build` - Build (debug only)
- `cargo test` - Run tests
- `cargo fmt` - Format code (run after changes)
- `cargo clippy` - Lint (run after changes)

## Dependencies

- **MUST use `cargo add <crate>` to add dependencies** - never manually edit Cargo.toml for dependencies
- **ALWAYS ask the user for approval before adding any new dependency**
- Only suggest well-known, widely-used crates with good maintenance records
- Prefer crates from the Rust ecosystem's trusted maintainers (e.g., tokio-rs, serde-rs, rust-lang)
- Check crate download counts and recent activity as indicators of reliability
- **ALWAYS run `cargo audit` after adding a new dependency**
- **ALWAYS report any vulnerabilities or issues from `cargo audit` to the user** - never silently ignore audit results

## Toolchain

- Prefer stable Rust
- Use nightly only if required

## Style

- Minimal comments, self-documenting code
- Concise implementations
- Discuss large refactors before starting

## Error Handling

- Use judgment: `unwrap`/`expect` OK when clearer
- Match on errors when recovery is needed

## Testing

- Unit tests with `#[cfg(test)]` module when asked
- Integration tests in separate files

## Build

- **NEVER** make release builds (`--release`)
- Always use debug builds
