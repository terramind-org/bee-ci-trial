# Installing Bee

Bee ships four binaries: `bee` (the CLI), `bee-server`, `bee-daemon`, and
`bee-github`.
Release builds exist for Linux x86_64, Linux aarch64 and macOS arm64. The Linux builds are made on
Ubuntu 22.04 and need glibc 2.35 or newer.

## Install script

```sh
curl -fsSL https://raw.githubusercontent.com/terramind-org/bee/main/scripts/install.sh | sh
```

The script detects your platform, downloads the release archive and its `.sha256`, checks the
checksum, and installs all four binaries. If the checksum does not match, nothing is installed.

| Variable | Default | Meaning |
|---|---|---|
| `BEE_VERSION` | latest release | Version to install, `0.1.0` or `v0.1.0`. |
| `BEE_INSTALL_DIR` | `$HOME/.local/bin` | Where the binaries go. |
| `BEE_DOWNLOAD_BASE` | `https://github.com/terramind-org/bee/releases/download` | Release download base; the script fetches `<base>/v<version>/bee-<version>-<target>.tar.gz`. Point it at a mirror. |

The script prints a `PATH` line if the install directory is not already on it.

## Manual download

Each release has, per target, `bee-<version>-<target>.tar.gz` and a matching `.sha256`, plus a
combined `SHA256SUMS`. Targets are `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and
`aarch64-apple-darwin`.

```sh
v=0.1.0; t=aarch64-apple-darwin
base=https://github.com/terramind-org/bee/releases/download/v$v
curl -fsSLO $base/bee-$v-$t.tar.gz
curl -fsSLO $base/bee-$v-$t.tar.gz.sha256
shasum -a 256 -c bee-$v-$t.tar.gz.sha256     # Linux: sha256sum -c
tar -xzf bee-$v-$t.tar.gz
install -m 755 bee-$v-$t/bee* ~/.local/bin/
```

The archive also holds `LICENSE`, `README.md`, and `NOTICE` when present.

## Container images

No release binaries or images have been published yet. When a release is published, container
images will be available at `ghcr.io/terramind-org/bee-server` and
`ghcr.io/terramind-org/bee-web`:

```sh
docker pull ghcr.io/terramind-org/bee-server:0.1.0   # server, CLI and daemon; linux/amd64, linux/arm64
docker pull ghcr.io/terramind-org/bee-web:0.1.0
```

## Building from source

You need the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it on first use) and
`protoc` on `PATH`.

```sh
scripts/dev-setup.sh
cargo build --release --locked -p bee-cli -p bee-server -p bee-daemon -p bee-github
ls target/release/bee target/release/bee-server target/release/bee-daemon target/release/bee-github
```

To build the images yourself, from the repository root:

```sh
docker build -t bee-server .
docker build -f apps/web/Dockerfile -t bee-web .
```

To build the same archive the release workflow produces:

```sh
scripts/package-release.sh "$(rustc -vV | sed -n 's/^host: //p')" 0.1.0
```

## How a release is cut

Tag the default branch as `v<version>` to start the release workflow. It builds the four Bee
binaries for Linux x86_64, Linux aarch64, and macOS arm64; packages each target as a compressed
archive and checksum; builds the server and web images for amd64 and arm64; publishes the images
at their version and `latest` tags; signs their digests with keyless cosign; and creates a GitHub
release containing the archives, their sidecars, and `SHA256SUMS`.

Verify a downloaded archive against the combined checksum file before unpacking it:

```sh
sha256sum -c SHA256SUMS             # macOS: shasum -a 256 -c <archive>.sha256
```

Verify an image signature against its immutable digest (replace the placeholder with the digest
reported by your container registry):

```sh
cosign verify ghcr.io/terramind-org/bee-server@sha256:<digest>
```

The release workflow currently lives in `ci/workflows/release.yml`; move it to
`.github/workflows/` before enabling releases on GitHub.
