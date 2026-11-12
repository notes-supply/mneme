# Deploy

How the server's image is built and published, how a cluster running Flux can follow it, and how releases are made. The README says how to run a server by hand.

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

A tag `vX.Y.Z` on a commit on `main` runs `.github/workflows/binaries.yml`. It builds a static `mneme` for x86_64 and aarch64 in the `Containerfile`'s build stage, checks that each is static, built for its machine and of the tag's version, signs each keyless as that workflow at that tag, and attaches them to a GitHub release with `SHA256SUMS`. `install.sh` and the README's [Verify a download](../README.md#verify-a-download) check those signatures.

It then tags the image CI published for the tag's commit with the version, and as `latest` when the tag is the newest version. It fails, and says so, when that commit has no image, which happens when another commit reached `main` before its release job ran; tag a commit that has one. The signature is on the image's digest, so the new tags need none of their own. With `TAG` a commit or a version:

```sh
cosign verify ghcr.io/notes-supply/mneme:TAG \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity https://github.com/notes-supply/mneme/.github/workflows/release.yml@refs/heads/main
```
