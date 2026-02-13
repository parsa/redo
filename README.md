# redo (Rust)

Rust implementation of **redo**, a build system with dynamic dependency discovery.

- **Specification**: D. J. Bernstein’s original description is at `https://cr.yp.to/redo.html`.
- **Status**: focuses on matching established redo semantics (including atomic output + honest prerequisites).

## Repository layout

- `bins/`: CLI binaries (`redo`, `redo-ifchange`, `redo-log`, …)
- `redo_core/`: core library used by the binaries
- `scripts/`: self-contained integration tests (no external repos required)

## Build

From the repo root:

```bash
cargo build --workspace
```

Binaries are produced under `target/debug/` (or `target/release/` if you build with `--release`).

## Install (local)

Install the binaries from source:

```bash
cargo install --path bins --bins
```

## Test

Run unit tests + the repo’s self-contained integration tests:

```bash
bash scripts/run_tests.sh
```

## Bulk dependency inputs (redo-ifchange)

When a tool produces a dependency list that can’t be safely shell-split (e.g. paths
with spaces), `redo-ifchange` can read targets from a file (or stdin) and treat
them exactly like positional arguments:

- `redo-ifchange --from-file <path>`: newline-separated targets (`-` = stdin)
- `redo-ifchange --from-file0 <path>`: NUL-separated targets (`-` = stdin)

### Optional: vendored integration tests (Apache-2.0)

This repo includes an Apache-2.0-licensed integration test suite under `third_party/python_redo_tests/`.
To run it against the Rust binaries:

```bash
bash scripts/run_vendored_python_tests.sh
```

You can override the suite timeout with `REDO_TEST_TIMEOUT_SECS`.

## License

- **Rust implementation**: Boost Software License 1.0 (see `LICENSE`).
- **Vendored tests (if present)**: files under `third_party/python_redo_tests/` are Apache-2.0 (see that directory’s `LICENSE`).

