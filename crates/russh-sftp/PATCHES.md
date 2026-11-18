# Local russh-sftp patch

Source: crates.io `russh-sftp` 3.0.1 (`AspectUnk/russh-sftp` commit `e8de8f903cb019f32beb26cc1195c7c71bef121f`). This copy retains the published library source, README, and Apache-2.0 license, omitting examples, benchmarks, and their dev dependencies.

- Format SFTP `longname` modification dates with Jiff in the existing UTC `%b %d %Y %H:%M` format.
- Convert `SystemTime` to on-wire `u32` seconds using the standard library, preserving floor rounding before the Unix epoch and the existing 32-bit wraparound. Public types and serialization are unchanged.
- Retain `AtomicU64::fetch_update` with a local deprecation allowance for compilers that predate `try_update`.

Remove the root `[patch.crates-io]` entry after upgrading to an upstream release without `chrono`; verify with `cargo tree --locked --target all --all-features -i chrono`.
