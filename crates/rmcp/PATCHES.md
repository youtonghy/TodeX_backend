# Local rmcp patch

Source: crates.io `rmcp` 3.9.0 (`modelcontextprotocol/rust-sdk` commit `5596a8a0bba2a1ec5519cc855cdadf23e4dcecdc`, `crates/rmcp`). This copy retains the published library source, README, and Apache-2.0 license. It omits the upstream integration suite, changelog, and the build script that installs Git hooks.

- Replace `chrono` with Jiff for task and annotation timestamps and Unix millisecond clock reads. The two typed `Annotations` constructors now accept `jiff::Timestamp`. MCP JSON remains string-valued: received timestamps are preserved verbatim, and generated timestamps keep the explicit `+00:00` offset and 0, 3, 6, or 9 fractional digits.
- Remove `schemars`'s `chrono04` feature; the affected timestamp fields are already modeled as strings. Timeouts still carry `std::time::Duration` and keep the previous diagnostic format.

To retire this patch, remove its root `[patch.crates-io]` entry when upgrading to an upstream release without a `chrono` dependency and check `cargo tree --locked --target all --all-features -i chrono`. Cargo can still retain `chrono` in the lockfile for `serde_with`'s inactive optional integration; it must not appear in the active tree.
