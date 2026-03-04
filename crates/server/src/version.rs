// SPDX-License-Identifier: BSD-3-Clause

/// The commit hash of the application.
pub const GIT_COMMIT_HASH: &str = env!("GIT_COMMIT_HASH");

/// The name of the branch the application was built from.
pub const GIT_BRANCH_NAME: &str = env!("GIT_BRANCH_NAME");

/// The version of the application.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
