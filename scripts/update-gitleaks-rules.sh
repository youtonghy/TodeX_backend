#!/bin/sh
# Replaces the embedded gitleaks rules (src/privacy_filter/gitleaks.toml)
# with those of a gitleaks release, then runs the privacy filter tests.
# Usage: scripts/update-gitleaks-rules.sh [vX.Y.Z]  (default: latest release)
set -eu
cd "$(dirname "$0")/.."
version="${1:-}"
if [ -z "$version" ]; then
  version=$(curl -fsSL https://api.github.com/repos/gitleaks/gitleaks/releases/latest |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
fi
base="https://raw.githubusercontent.com/gitleaks/gitleaks/$version"
curl -fsSL "$base/config/gitleaks.toml" -o src/privacy_filter/gitleaks.toml
curl -fsSL "$base/LICENSE" -o src/privacy_filter/LICENSE-gitleaks
sed -i.bak "s/gitleaks v[0-9.]*, MIT/gitleaks $version, MIT/" src/privacy_filter/gitleaks.rs
rm -f src/privacy_filter/gitleaks.rs.bak
echo "gitleaks rules updated to $version; update the version in docs/API.md too."
cargo test --bin todex-agentd privacy_filter
