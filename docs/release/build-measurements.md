# Release build measurements

This record measures the Linux release build of the four distributable binaries.
All commands below ran in the same sandbox with `CARGO_BUILD_JOBS=2`.

## Environment

| Item | Measured value | Command |
| --- | --- | --- |
| Logical cores | 2 | `nproc` |
| Memory | 17,179,869,184 bytes (16 GiB) | `free -b` |
| Rust compiler | `rustc 1.99.0 (b940084d7 2026-09-28)` | `rustc --version` |
| Host target | `x86_64-unknown-linux-gnu` | `rustc -vV` |

## Release profile

`Cargo.toml` configures `strip = true` and `lto = "thin"`. It deliberately
retains Cargo's default 16 codegen units, as documented beside the profile.

## Build result

The measured build began at Unix timestamp `1795013700` with:

```sh
env CARGO_BUILD_JOBS=2 cargo build --release --locked \
  -p bee-cli -p bee-server -p bee-daemon -p bee-github
```

| Metric | Result | Command |
| --- | --- | --- |
| Status | Success (`BUILD-EXIT=0`) | `cargo build --release --locked ...` |
| Cargo elapsed time | 37m 09s | Cargo completion output |
| Timestamp elapsed time | 2,233 seconds (37m 13s) | `cat /tmp/build-start; date -u +%s` |
| Largest sampled `rustc` RSS | 6,769,452 KiB | `ps -eo rss,comm --sort=-rss \| head -12` |
| `bee` | 39,213,272 bytes | `stat -c '%n %s bytes' target/release/bee` |
| `bee-server` | 158,354,568 bytes | `stat -c '%n %s bytes' target/release/bee-server` |
| `bee-daemon` | 30,937,392 bytes | `stat -c '%n %s bytes' target/release/bee-daemon` |
| `bee-github` | 34,382,648 bytes | `stat -c '%n %s bytes' target/release/bee-github` |
| Package archive | 85,380,705 bytes | `stat -c '%n %s bytes' dist/bee-0.1.0-x86_64-unknown-linux-gnu.tar.gz` |

`/usr/bin/time` is absent on this host, so RSS was sampled from `ps` while the
build ran. The reported peak is the largest observed compiler RSS.

## Package and install verification

The archive was created with:

```sh
scripts/package-release.sh x86_64-unknown-linux-gnu 0.1.0 target/release
```

Its SHA-256 was `1e1f37daf13687c4f377434fa8f72988290ee1e08d9a0e0a219c3571c077c849`.
The archive and its checksum sidecar were copied under
`v0.1.0/` in a temporary release-layout directory and served by
`python3 -m http.server 18080`. A request for the archive returned
`HTTP/1.0 200 OK`.

The installer ran with `BEE_DOWNLOAD_BASE=http://127.0.0.1:18080`,
`BEE_VERSION=0.1.0`, and a temporary `BEE_INSTALL_DIR`:

```text
Downloading bee-0.1.0-x86_64-unknown-linux-gnu.tar.gz
Checksum ok (1e1f37daf13687c4f377434fa8f72988290ee1e08d9a0e0a219c3571c077c849)
Installed /tmp/bee-install-measure-1795015933/bee
Installed /tmp/bee-install-measure-1795015933/bee-server
Installed /tmp/bee-install-measure-1795015933/bee-daemon
Installed /tmp/bee-install-measure-1795015933/bee-github
Bee 0.1.0 installed for x86_64-unknown-linux-gnu.
bee 0.1.0
```

The installer exited 0, installed `bee --version` exited 0 and printed
`bee 0.1.0`, and installed `bee-server --help` exited 2. Its first
line was:

```text
usage: bee-server [--config <file>] [--dev [--temp]] [--listen <addr>] [--data-dir <dir>]
```
