# Install

Mneme runs on Linux, on x86_64 and aarch64, and needs Git 2.45 or newer. Each release is a static binary for each architecture, with `SHA256SUMS` and a cosign bundle per binary, signed keyless by the workflow that built it.

## The installer

```sh
curl -fsSL https://raw.githubusercontent.com/notes-supply/mneme/main/install.sh | sh
```

installs the latest `mneme` and [Atrium](https://github.com/notes-supply/atrium)'s `atrium` into `~/.local/bin`, without sudo. It checks each binary against its release's `SHA256SUMS`, and against its signature when [cosign](https://docs.sigstore.dev/cosign/system_config/installation/) is on `PATH`, and installs none unless all pass. Into `~/.local/bin`, on a machine with `systemctl`, it also installs Atrium's systemd user unit when there is none yet, without enabling it. It says when that folder is not on `PATH`, which it must be, or Git is too old, and when it replaced a binary that a running service still uses.

When `curl` fails, the pipe gives `sh` nothing to read and `sh` exits 0, so a failed download can look like a success. To be sure, download the script first:

```sh
curl -fsSL https://raw.githubusercontent.com/notes-supply/mneme/main/install.sh -o install.sh && sh install.sh
```

- `sh install.sh mneme`, or `sh -s -- mneme` after the pipe, installs Mneme alone, and `atrium` Atrium alone.
- `MNEME_INSTALL_DIR=/some/folder` installs there instead, and installs no unit.

## Without a systemd user manager

Over SSH, in a container, or on a machine without systemd, there is no user manager to run the services. `mneme init --no-service` makes the folder ready and prints the `mneme autosave run` command, for whatever keeps processes running there; `mneme init` does the same by itself when `systemctl` is not on `PATH`. Atrium runs in a terminal with `atrium --wiki-folder ~/wiki`.

## mise

```sh
mise use -g github:notes-supply/mneme github:notes-supply/atrium
```

mise keeps each version in a folder of its own, and the autosave unit runs Mneme by its full path, so run `mneme init --force` with the same options after an upgrade. Recent versions of mise take a release only once it is a day old; name the version, as `github:notes-supply/mneme@0.1.0`, to take it sooner.

## Verify a download

To check a binary by hand, with cosign, before installing it:

```sh
version=v0.1.0
file=mneme-$(uname -m)-linux
base=https://github.com/notes-supply/mneme/releases/download/$version
curl -fL --remote-name-all "$base/$file" "$base/$file.sigstore.json" &&
  cosign verify-blob "$file" --bundle "$file.sigstore.json" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    --certificate-identity "https://github.com/notes-supply/mneme/.github/workflows/binaries.yml@refs/tags/$version" &&
  install -D -m 0755 "$file" ~/.local/bin/mneme
```

Each step runs only when the one before it passed, so a binary whose signature does not check out is never installed. Atrium's are checked the same way, with `atrium` in place of `mneme`.

## From source

```sh
cargo install --locked --root ~/.local --git https://github.com/notes-supply/mneme --tag v0.1.0
```
