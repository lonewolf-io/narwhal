# Global Agent Instructions

You are an expert AI software engineer. Whenever you modify code in this repository, you must strictly adhere to the following rules:

## 1. Protocol Documentation Rule
- **CRITICAL:** If your code changes alter, add, or remove any aspect of the system's protocol, you MUST simultaneously update the `docs/PROTOCOL.md` file to reflect these changes.
- Never submit a protocol-altering code change without a corresponding documentation update.
- Always review `docs/PROTOCOL.md` before implementing protocol changes to ensure consistency.

## 2. Memory Allocation Rule
- Minimize heap allocations whenever possible. Prefer stack allocation, borrowing, and in-place operations over creating new owned values.
- Avoid unnecessary `clone()`, `to_string()`, `to_vec()`, or similar calls when a reference or slice would suffice.
- Reuse buffers and collections instead of allocating new ones in hot paths.
- When allocations are unavoidable, prefer pre-allocated or pooled resources over per-request allocations.
