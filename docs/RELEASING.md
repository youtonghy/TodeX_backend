# Releasing `todex-agentd`

Backend releases are created manually from **Actions > Release backend binaries**.

Enter a stable semantic version such as `1.2.3` (an optional leading `v` is
accepted). The workflow tags the selected source commit, injects the version at
compile time, runs the backend checks, and builds native Linux x64, macOS ARM64,
and Windows x64 binaries. It verifies their embedded version and publishes all
archives plus `SHA256SUMS` to the `v1.2.3` GitHub Release.

Development builds report `DEV0.0.0`; Cargo metadata retains the valid placeholder
version `0.0.0` because the release version belongs to the CI build, not the source.

The Linux archive targets GNU/glibc systems. Every target machine must also have
the selected provider CLI installed and authenticated. See `BUILD_RUN.md` for
runtime configuration and deployment details.

## macOS code signing

The macOS binary is always signed; the release fails before building when the
signing certificate is missing. Configure these repository or organization
secrets (the same names the desktop release uses):

| Secret | Purpose |
| --- | --- |
| `MAC_CSC_LINK` | Base64-encoded `.p12` with exactly one code signing identity (required). |
| `MAC_CSC_KEY_PASSWORD` | Password of that `.p12`. |
| `APPLE_ID` | Apple ID used for notarization (Developer ID only). |
| `APPLE_APP_SPECIFIC_PASSWORD` | App-specific password for that Apple ID. |
| `APPLE_TEAM_ID` | Team ID of the Developer ID certificate. |

The workflow imports the certificate into a temporary keychain (deleted at the
end of the job), signs `todex-agentd` with the hardened runtime and the fixed
identifier `com.unbaked0692.todex.agentd`, verifies the signature, and only then
packages it, so the archive, the raw `.bin` update asset, and `SHA256SUMS` all
contain the signed binary.

- **Developer ID Application certificate**: the signature must carry Apple's
  secure timestamp. When all three `APPLE_*` secrets are set, the binary is also
  notarized and the release fails unless Apple accepts it; without them the job
  warns and skips notarization. A bare executable cannot be stapled, so
  Gatekeeper looks the notarization ticket up online when it checks a
  quarantined copy.
- **Self-signed certificate**: the runner trusts it for code signing, as the
  desktop release does, and there is no notarization. The workflow requests a
  secure timestamp but falls back to `--timestamp=none` with a warning if
  Apple's timestamp server rejects or cannot be reached, since TCC grants do not
  depend on it. Users who download the archive in a browser must clear the
  quarantine attribute themselves; `install.sh` and the built-in updater download
  without quarantine.

**Keep the same certificate.** The daemon performs Computer Use itself, which
needs Screen Recording and Accessibility permissions. macOS (TCC) records those
grants for a bare executable by its path and its signature's designated
requirement (`identifier "com.unbaked0692.todex.agentd" and certificate leaf =
…` for a self-signed certificate; identifier plus team ID for Developer ID). As
long as every release is signed by the same certificate with the same identifier
and is installed at the same path (by default `~/.local/bin/todex-agentd`, which
`install.sh` and the auto-updater replace in place), the grants survive updates.
An unsigned or ad-hoc signed build, a different or renewed self-signed
certificate, or a move to another path makes macOS treat the daemon as a new
app, and users must grant both permissions again.

## Failed releases

The workflow refuses to replace an existing tag or Release. Run it only from the
commit that should be tagged, and use a new version when publishing a new build.
Assets are uploaded to a draft and checked before it becomes public. A failed
job cleans up the draft and release tag automatically; if GitHub is unavailable
during cleanup, delete the remaining draft Release and tag before retrying the
same version.
