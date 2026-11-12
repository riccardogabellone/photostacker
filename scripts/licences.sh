#!/usr/bin/env bash
# The licences of the Rust crates the packages ship, with their texts (spec §11.3), written
# with cargo-about to dist/package/resources/licenses/THIRD-PARTY-LICENSES.md, where a
# package's licences folder is staged. `just licences` and scripts/package.sh both run it.
#
# Usage (bash on Linux and macOS, Git Bash on Windows): scripts/licences.sh
#
# It needs cargo-about 0.9.3, the version the release workflow builds
# (`cargo install cargo-about --version 0.9.3 --locked --features cli`), which reads
# about.toml and the template in packaging/licenses, and the network: it fetches from
# GitHub the licence files that some crates leave out of their package (about.toml's `git`
# clarifications).
set -euo pipefail

if [ "$#" != 0 ]; then
    echo "usage: $0" >&2
    exit 2
fi

# cargo reads .cargo/config.toml from the folder it starts in.
cd "$(dirname "${BASH_SOURCE[0]}")/.."

mkdir -p dist/package/resources/licenses
cargo about generate --workspace --locked --fail --config about.toml \
    --output-file dist/package/resources/licenses/THIRD-PARTY-LICENSES.md \
    packaging/licenses/about.hbs
