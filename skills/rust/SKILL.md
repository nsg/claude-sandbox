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

- Use cargo commands only
- **ALWAYS ask the user for approval before adding any new dependency**
- Only suggest well-known, widely-used crates with good maintenance records
- Prefer crates from the Rust ecosystem's trusted maintainers (e.g., tokio-rs, serde-rs, rust-lang)
- Check crate download counts and recent activity as indicators of reliability
- Run `cargo audit` to check for vulnerabilities

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
