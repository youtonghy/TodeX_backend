#!/usr/bin/env bash
# Cargo runner for macOS (see .cargo/config.toml): signs the freshly linked
# todex-agentd with the same identifier and flags as the release workflow, then
# runs it. A plain `cargo run` leaves only a linker ad-hoc signature whose hash
# changes on every build, so macOS drops the Screen Recording and Accessibility
# grants Computer Use needs each time. Signed with one stable certificate, the
# grants stay valid for target/debug/todex-agentd across rebuilds.
#
# Identity: $TODEX_DEV_SIGN_IDENTITY (certificate name or SHA-1), otherwise the
# only valid code signing identity in the keychains. With none to pick, the
# binary runs unsigned and a warning says so. Other binaries (test harnesses)
# run untouched.
set -euo pipefail

binary="$1"
shift

identifier="com.unbaked0692.todex.agentd"

if [[ "$(basename "${binary}")" == "todex-agentd" ]]; then
  identity="${TODEX_DEV_SIGN_IDENTITY:-}"
  if [[ -z "${identity}" ]]; then
    identities="$(security find-identity -v -p codesigning | sed -nE 's/^ *[0-9]+\) ([0-9A-F]{40}) .*$/\1/p')"
    if [[ "$(grep -c . <<<"${identities}" || true)" -eq 1 ]]; then
      identity="${identities}"
    fi
  fi

  if [[ -z "${identity}" ]]; then
    echo "warning: TODEX_DEV_SIGN_IDENTITY is not set and there is not exactly one code signing identity; running unsigned, so macOS Computer Use permissions will not persist." >&2
  else
    # Captured first: `grep -q` closing the pipe early would trip pipefail.
    details="$(codesign -dv "${binary}" 2>&1 || true)"
    if ! grep -qx "Identifier=${identifier}" <<<"${details}" || grep -qE '^Signature=adhoc|linker-signed' <<<"${details}"; then
      codesign --force --options runtime --identifier "${identifier}" \
        --timestamp=none --sign "${identity}" "${binary}" >&2
    fi
  fi
fi

exec "${binary}" "$@"
