# Development

[mise](https://mise.jdx.dev) installs every tool at the version `mise.toml` pins, from `mise.lock`, and runs every task. `mise install` in the checkout fetches what is missing.

## Check it

```sh
mise run gate
```

The gate checks formatting, runs clippy with warnings denied, runs the tests with nextest, checks the RustSec advisories, lints the CI workflows with actionlint and zizmor, and lints the shell scripts with shellcheck. CI runs the same command. Set `MISE_OFFLINE=1` to check the advisories against the local copy of the database without fetching it.

The tests include a simulation of three clones and a server over 128 seeds, and the same against a bare repository. `mise run soak` runs 2000 seeds of each and the kill test over 20. They also pipe `install.sh` to `sh` against a fake release served on loopback, for a good install, a bad checksum, an unknown architecture and the cosign check.

`mise run image` builds the server's image and checks it with a real clone and push, the autosave service run from the image against it, and a Trivy scan (see [deploy.md](deploy.md)).

## Build it

```sh
cargo build --locked --release
cargo install --locked --root ~/.local --git https://github.com/notes-supply/mneme --tag v0.1.0
```

The release binaries are built in the `Containerfile`'s build stage instead, static and with stable Rust, and the gate uses the nightly `mise.toml` names. [decisions.md](decisions.md) says why.

## Screenshots

```sh
MNEME_PLAYWRIGHT=../atrium MNEME_CHROMIUM=$(command -v chromium) mise run screenshots
```

remakes `docs/images/` from a real run with invented content: two machines, `laptop` and `desktop`, as containers of the server's image that sync with a third, and Atrium v0.1.0 reading the laptop's folder. `MNEME_PLAYWRIGHT` names a folder whose `node_modules` has Playwright, such as a checkout of Atrium after `npm ci`, and `MNEME_CHROMIUM` a Chromium to use instead of Playwright's own. `scripts/screenshots.sh` says what is real in them.

## Release

A push to `main` deploys, and a tag `vX.Y.Z` on a commit on `main` makes a release with signed binaries and tags the image with the version. Bump `version` in `Cargo.toml` first: the release refuses a binary whose `--version` is not the tag's. [deploy.md](deploy.md) has the details.
