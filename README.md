# Mneme

Mneme keeps a Markdown wiki in Git, so that writers only edit files. `mneme autosave` runs on each writer's machine: it commits what has been still for a while, syncs with the other machines through a Git remote, and pushes. The remote can be any Git repository, such as a bare one over SSH or one on a Git host. An optional server, `mneme serve`, can hold the repository instead. It adds a push check that keeps the wiki's rules, a token per machine, each machine's state, and a reading API with pages, history, diffs and search.

## Install

Mneme runs on Linux, on x86_64 and aarch64, and needs Git 2.45 or newer.

Each release has a static binary for each architecture, signed keyless by the workflow that built it. Download one for the release you want, check it with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/), and install it to `~/.local/bin`:

```sh
version=v0.1.0
file=mneme-$(uname -m)-linux
curl -fL --remote-name-all \
  "https://github.com/notes-supply/mneme/releases/download/$version/$file" \
  "https://github.com/notes-supply/mneme/releases/download/$version/$file.sigstore.json"
cosign verify-blob "$file" --bundle "$file.sigstore.json" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity "https://github.com/notes-supply/mneme/.github/workflows/binaries.yml@refs/tags/$version"
install -D -m 0755 "$file" ~/.local/bin/mneme
```

Each release also has `SHA256SUMS`. With Rust, build it instead:

```sh
cargo install --locked --root ~/.local --git https://github.com/notes-supply/mneme --tag v0.1.0
```

## Quick start

`mneme init` makes a wiki folder ready, installs the autosave service as a systemd user unit, and starts it. Run it once on each machine. The folder is `~/wiki` unless `--folder` names another, and the machine's name, which its autosave commits carry, is the short host name unless `--name` gives one. A missing or empty folder gets a clone. A folder of notes that is not a Git repository yet becomes one when the remote has no history, and the first save pushes the notes. The service's logs are in `journalctl --user -u mneme-autosave`. With `--no-service`, `mneme init` prints the command that runs the service instead of installing it.

### A bare repository over SSH

Any machine you reach with `ssh` can hold the wiki, through keys or Tailscale SSH. Make the repository once:

```sh
ssh host git init --bare -b main wiki.git
```

Then on each machine:

```sh
mneme init --remote ssh://host/~/wiki.git
```

The service runs your own `ssh`, so `~/.ssh/config`, your keys and Tailscale SSH work as they do in a terminal. It never answers a question: it needs a key without a passphrase, an agent that the service can reach, or Tailscale SSH, and only `mneme init` may learn a new host key.

### A Git host with a deploy key

Give each machine a key of its own and add its public half to the repository as a deploy key with write access:

```sh
ssh-keygen -t ed25519 -N '' -f ~/.ssh/wiki-deploy
mneme init --remote git@github.com:someone/wiki.git --ssh-key ~/.ssh/wiki-deploy
```

ssh then uses that key alone. Over HTTPS, `--token-file` takes a file that holds an access token instead, which is sent as the password of HTTP Basic authentication.

### A Mneme server

```sh
mneme init --server https://mneme.example --token-file ~/.config/mneme/token
```

The token file holds this machine's token and nothing else, and the server's tokens file names the machine as `--name` does (see [The server](#the-server)).

## Reading the wiki locally

```sh
mneme serve --folder ~/wiki
```

serves the [reading API](docs/api.md) for the clone on this machine at `http://127.0.0.1:8081`: pages, history, diffs and search, from the folder's `main`, which it follows as the service syncs. It writes nothing, listens on loopback only, and answers only requests addressed to `localhost`, `127.0.0.1` or `[::1]`. [Atrium](https://github.com/notes-supply/atrium) runs it for you.

## The server

```sh
printf '{ "laptop": "%s" }\n' "$(printf %s "$TOKEN" | sha256sum | cut -d' ' -f1)" > tokens.json
mneme serve --repo /data/wiki.git --tokens tokens.json --addr 0.0.0.0:8080
git -c "http.extraHeader=Authorization: Bearer $TOKEN" clone http://host:8080/git/wiki.git
curl http://host:8080/v1/pages
```

The tokens file maps each machine's name to the SHA-256 of its token. The server creates the repository when it is missing and installs the push check (`mneme hook pre-receive`) into it. Machines sync over `/git/wiki.git` with their tokens, and each autosave service reports its state there, which `/v1/machines` lists. The server needs Git 2.45 or newer on `PATH`, with `git http-backend`, and refuses to start with an older one. It listens on `127.0.0.1:8080` unless `--addr` says otherwise. Logs are JSON lines on standard error. SIGTERM stops it.

`/v1` has no login: whoever reaches it reads the whole wiki. Beyond the people who may read all of it, expose only `/git/`. On a tailnet, `tailscale serve` in front of the server is enough. The reading API is described in [docs/api.md](docs/api.md).

`mise run image` builds the server's image from the `Containerfile`, a static `mneme` beside Alpine's Git, and checks it with a real clone and push and a Trivy scan. It runs as UID 65532 on a read-only root, keeps the repository on a volume at `/data` and reads the tokens from `/etc/mneme/tokens.json`. CI builds it the same way on every push and publishes it only from `main` (see [Deploy](#deploy)).

## What a plain remote gives up

A plain Git remote stores what it is sent. There is no push check on the remote side: the service never saves a file the server would refuse, but a hand commit that holds one, or a push from elsewhere, gets in. There is no per-machine status either: no `/v1/machines`, and a machine's log is the only place that says it is blocked or which files it leaves out.

## Autosave

Each writer's machine runs `mneme autosave` on its wiki folder. Writers only edit files. The service commits what has been still for 15 seconds, fetches, replays its commits on top of the server's, and pushes, every few seconds and without ever waiting for a person. Where two machines changed the same lines, the later one keeps its own text and marks its commit with an `Overlap:` trailer. It waits instead when the folder is in the middle of a Git operation it did not start, and stops saving when a save would delete most pages. A file the server would refuse, such as a symlink or a page over 1 MiB, stays in the folder and out of every save, and the service reports it.

`mneme init` writes the unit with the command it runs, which is one of:

```sh
mneme autosave run --folder ~/wiki --remote ssh://host/~/wiki.git --name laptop
mneme autosave run --folder ~/wiki --server https://host --token-file ~/.config/mneme/token --name laptop
```

`mneme autosave init`, with the same flags, clones into a missing or empty folder, or checks an existing clone. A folder takes one service: a second `run` on it exits.

## Deploy

A push to `main` publishes Mneme once the gate passes on that commit. CI's release job pushes the image built by the `image` job to `ghcr.io/notes-supply/mneme` by digest. `scripts/release.sh` then packs `deploy/` with that digest as one OCI artifact, `ghcr.io/notes-supply/manifests/mneme`, signs the image and the artifact keyless as `.github/workflows/release.yml` on `main`, and points the artifact's `main` tag at it. A cluster running Flux can follow that tag and verify the signature:

```yaml
apiVersion: source.toolkit.fluxcd.io/v1
kind: OCIRepository
metadata:
  name: mneme
spec:
  interval: 5m
  url: oci://ghcr.io/notes-supply/manifests/mneme
  ref:
    tag: main
  verify:
    provider: cosign
    matchOIDCIdentity:
      - issuer: ^https://token\.actions\.githubusercontent\.com$
        subject: ^https://github\.com/notes-supply/mneme/\.github/workflows/release\.yml@refs/heads/main$
```

`deploy/` is a Kustomize base that holds only what a release may change: the Deployment. The cluster's own repository adds its specifics as an overlay, and owns the Service, the ingress, the network policy, the tokens Secret `mneme-tokens` and the volume claim `mneme-repository`, which the Deployment mounts by name. The project's own cluster does exactly that. Mneme reads the tokens file at start, so whatever changes the Secret should restart it.

The Deployment asks for little, so that a locked-down namespace takes it, and a release should keep it so: it runs as the `default` service account with no token mounted, mounts no inline CSI volume, reads no Secret but `mneme-tokens`, and names no pull secret, since the image is public. A pod the cluster refuses is never created, so that release never gets ready, which leaves Mneme down until a revert.

Roll back a bad release with a revert commit on `main`; it deploys the same way. One pod holds the volume, so a release whose pod never gets ready leaves Mneme down until Flux applies the revert, once that release's health check has timed out. Never retag the artifact by hand: a cluster that verifies accepts only what the release workflow signed on `main`.

A tag `v*` on a commit on `main` publishes the release binaries (see [Install](#install)) from `.github/workflows/binaries.yml`.

## Check it

```sh
mise run gate
```

The gate checks formatting, runs clippy with warnings denied, runs the tests with nextest, checks the RustSec advisories, and lints the CI workflows. Its tests include a simulation of three clones and a server over 128 seeds, and the same against a bare repository. `mise run soak` runs 2000 seeds of each. CI runs the same command. Set `MISE_OFFLINE=1` to check the advisories against the local copy of the database without fetching it.

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Mneme by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
