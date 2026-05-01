# Global Agent Instructions

You are an expert AI software engineer. Whenever you modify code in this repository, you must strictly adhere to the following rules:

## 1. Protocol Documentation Rule
- **CRITICAL:** If your code changes alter, add, or remove any aspect of the system's protocol, you MUST simultaneously update the `docs/PROTOCOL.md` file to reflect these changes.
- Never submit a protocol-altering code change without a corresponding documentation update.
- Always review `docs/PROTOCOL.md` before implementing protocol changes to ensure consistency.

## 2. Memory Allocation & Idiomatic Rust Rule
- Minimize heap allocations whenever possible. Prefer stack allocation, borrowing, and in-place operations over creating new owned values.
- Avoid unnecessary `clone()`, `to_string()`, `to_vec()`, `unwrap()`, or `expect()` calls when a reference, slice, or proper error handling would suffice.
- Reuse buffers and collections instead of allocating new ones in hot paths.
- When allocations are unavoidable, prefer pre-allocated or pooled resources over per-request allocations.

## 3. Rust Formatting & Linting Gates
Whenever you modify, create, or delete Rust (`.rs`) files, you MUST follow this exact validation loop before considering your task complete and returning control to the user:

1. **Format:** Run `cargo fmt --all` to format the workspace. Do not ask for permission, just run it.
2. **Lint:** Run `cargo clippy --workspace --all-targets -- -D warnings` (or use `-p <package>` if scoped to a specific crate) to check for idiomatic code and errors.
3. **Self-Correct:** If Clippy outputs ANY warnings or errors related to your changes:
   - You must read the error output.
   - You must apply the fixes suggested by Clippy.
   - You may use `cargo clippy --fix --allow-dirty --allow-staged` if it helps resolve the issues faster.
   - You must re-run the `cargo clippy` command to verify the fix worked.
4. **Completion:** Do not report that you are finished until `cargo fmt` has been run and `cargo clippy` returns a clean, zero-exit-code run with no warnings.

## 4. Comment Style Rule
Apply this rule only to comment text that you newly add or modify as part of the current change.
  - Do not use em dashes (—).
  - Do not use ellipses, whether written as `...` or `…`.
