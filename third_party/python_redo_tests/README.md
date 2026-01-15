# Vendored tests (Apache-2.0)

This directory contains third-party test cases and helpers.

- **License for everything under this directory**: Apache-2.0 (see `LICENSE` next to this file).
- These files are **not** covered by the repository’s top-level Boost license.
- Some files may have been modified to work with the Rust binaries and/or to remove non-portable paths.

If you modify files in this directory, keep them under Apache-2.0 and retain attribution as required by the Apache license.

Notes:
- `install.do` is a small, Rust-specific install target added so `t/999-installer/all.do` can run in this standalone repo.
