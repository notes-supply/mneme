# Deploy

How to run a Mneme server, how its image is built and published, how a cluster running Flux can follow it, and how releases are made.

## Run a server

```sh
printf '{ "laptop": "%s" }\n' "$(printf %s "$TOKEN" | sha256sum | cut -d' ' -f1)" > tokens.json
mneme serve --repo /data/wiki.git --tokens tokens.json --addr 0.0.0.0:8080
```

The tokens file maps each machine's name to the SHA-256 of its token, and each machine runs `mneme init --server URL --token-file FILE` with its own token and that name. An entry can also be an object, for a machine that only keeps a copy and needs no more than to fetch:

```json
{ "laptop": "9f86d081…", "mirror": { "sha256": "2c26b46b…", "access": "read" } }
```

A read-only token may clone, fetch and report its machine's state, and a push with it is refused with a reason Git shows. `access` is `write` when it is left out, as for a plain hash, and any other key or value is refused at start. `--tokens-env NAME` takes the same JSON from an environment variable instead of a file, as a container platform gives a secret. The server creates the repository when it is missing and installs its push check, which refuses what breaks the wiki's rules: a symlink, a submodule, a path named `.git`, a page over 1 MiB, a file over 10 MiB, or a deletion of `main`. Machines sync over `/git/wiki.git` with their tokens and report their state, which `/v1/machines` lists.

It needs Git 2.45 or newer on `PATH`, with `git http-backend`, and refuses to start with an older one. It listens on `127.0.0.1:8080` unless `--addr` says otherwise, logs JSON lines on standard error, and stops on SIGTERM.

By default `/v1`, the [reading API](api.md), has no login: whoever reaches it reads the whole wiki. Git over `/git/` needs a machine's token. So beyond the people who may read all of the wiki, expose only `/git/`. On a tailnet, `tailscale serve` in front of the server is enough to keep `/v1` to the tailnet.

A client that is not a folder of files, such as a web editor or an agent, changes pages through the server's [editing routes](api.md#editing) with a token that may write, with or without `--private-api`. Each change is one commit on `main`, made on top of whatever has been pushed, keeps the push check's rules, and is refused when the page itself has changed since the client read it.

With `--private-api`, every `/v1` route asks for `Authorization: Bearer <token>` with a token from the tokens file, read-only or not, and answers `401` with no body without one, as `/git/` does. `/healthz` stays open for probes. A server reachable from the internet should run with it.

## The image

`mise run image` builds the server's image from the `Containerfile`, a static `mneme` beside Alpine's Git, and checks it with a real clone and push and a Trivy scan. It runs as UID 65532 on a read-only root, keeps the repository on a volume at `/data` and reads the tokens from `/etc/mneme/tokens.json`. It runs `mneme serve --repo /data/wiki.git --tokens /etc/mneme/tokens.json --addr 0.0.0.0:8080`. CI builds it the same way on every push and publishes it only from `main`, as `ghcr.io/notes-supply/mneme:<commit>`. From the release after v0.1.0 on, each release also tags its commit's image with its version, such as `0.2.0`, and moves `latest` to it, so a server can pin a version.

## Every push to main

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

## A release

A tag `vX.Y.Z` on a commit on `main` runs `.github/workflows/binaries.yml`. It builds a static `mneme` for x86_64 and aarch64 in the `Containerfile`'s build stage, checks that each is static, built for its machine and of the tag's version, signs each keyless as that workflow at that tag, and attaches them to a GitHub release with `SHA256SUMS`. `install.sh` and [Verify a download](install.md#verify-a-download) check those signatures.

It then tags the image CI published for the tag's commit with the version, and a job of its own points `latest` at the newest version that has an image. The first fails, and says so, when that commit has no image, which happens when another commit reached `main` before its release job ran; tag a commit that has one. The signature is on the image's digest, so the new tags need none of their own. With `TAG` a commit or a version:

```sh
cosign verify ghcr.io/notes-supply/mneme:TAG \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity https://github.com/notes-supply/mneme/.github/workflows/release.yml@refs/heads/main
```
