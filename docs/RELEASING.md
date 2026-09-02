# Releasing `todex-agentd`

Backend releases are created manually from **Actions > Release backend binaries**.

Enter a stable semantic version such as `1.2.3` (an optional leading `v` is
accepted). The workflow updates `Cargo.toml` and `Cargo.lock` in a release-only
commit, tags that exact source, runs the backend checks, and builds native Linux
x64, macOS ARM64, and Windows x64 binaries. It verifies their embedded version
and publishes all archives plus `SHA256SUMS` to the `v1.2.3` GitHub Release.

The Linux archive targets GNU/glibc systems. Every target machine must also have
the selected provider CLI installed and authenticated. See `BUILD_RUN.md` for
runtime configuration and deployment details.

The workflow refuses to replace an existing tag or Release. Run it only from the
commit that should be tagged, and use a new version when publishing a new build.
Assets are uploaded to a draft and checked before it becomes public. A failed
job cleans up the draft and release tag automatically; if GitHub is unavailable
during cleanup, delete the remaining draft Release and tag before retrying the
same version.
