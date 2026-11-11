# Mneme

Mneme holds the wiki's Git repository. Writers edit Markdown files in their own clone, and an autosave service commits and pushes for them. Mneme serves the bare repository over HTTPS to those clones, refuses pushes that would break the wiki's rules, and answers read requests from the frontend with JSON: pages, history, diffs and search.

Git stores, syncs, merges and finds renames. Mneme keeps an in-memory SQLite index of the history, built at start and updated after each push.

## Run it

```sh
printf '{ "blackbox": "%s" }\n' "$(printf %s "$TOKEN" | sha256sum | cut -d' ' -f1)" > tokens.json
mneme serve --repo /data/wiki.git --tokens tokens.json --addr 0.0.0.0:8080
git -c "http.extraHeader=Authorization: Bearer $TOKEN" clone http://host:8080/git/wiki.git
curl http://host:8080/v1/pages
```

The tokens file maps each machine's name to the SHA-256 of its token. The server creates the repository when it is missing, and installs the push check (`mneme hook pre-receive`) into it. It needs Git 2.45 or newer on `PATH`, with `git http-backend`, and refuses to start with an older one. Logs are JSON lines on standard error. SIGTERM stops it.

The reading API is described in [docs/api.md](docs/api.md).

`mise run image` builds the server's image from the `Containerfile`, a static `mneme` beside Alpine's Git, and checks it with a real clone and push and a Trivy scan. It runs as UID 65532 on a read-only root, keeps the repository on a volume at `/data` and reads the tokens from `/etc/mneme/tokens.json`. CI builds it the same way on every push and publishes it only from `main` (see Deploy).

## Autosave

Each writer's machine runs `mneme autosave` on its wiki folder. Writers only edit files. The service commits what has been still for 15 seconds, fetches, replays its commits on top of the server's, and pushes, every few seconds and without ever waiting for a person. Where two machines changed the same lines, the later one keeps its own text and marks its commit with an `Overlap:` trailer. It waits instead when the folder is in the middle of a Git operation it did not start, and stops saving when a save would delete most pages. A file the server would refuse, such as a symlink or a page over 1 MiB, stays in the folder and out of every save, and the service reports it.

```sh
mneme autosave init --folder ~/wiki --server https://host --token-file ~/.config/mneme/token --name blackbox
mneme autosave run  --folder ~/wiki --server https://host --token-file ~/.config/mneme/token --name blackbox
```

`init` clones into a missing or empty folder, or checks an existing clone. Both need Git 2.45 or newer. A folder takes one service: a second `run` on it exits. [deploy/mneme-autosave.service](deploy/mneme-autosave.service) runs the service as a systemd user unit. It reports its state to the server, which lists every machine on `/v1/machines`.

## Deploy

A push to `main` deploys Mneme to Apollo once the gate passes on that commit. CI's release job pushes the image built by the `image` job to `ghcr.io/notes-supply/mneme` by digest, and `scripts/release.sh` packs `deploy/` with that digest as one OCI artifact, signs it keyless as `.github/workflows/release.yml` on `main`, and points its `main` tag at it. Flux on Apollo verifies that signature and applies `deploy/` in namespace `atrium`. `deploy/` holds only what a release may change: the Deployment. The cluster repository owns the Service, Ingress, network policy, the tokens Secret and the repository's volume, which the Deployment mounts by name.

Roll back a bad release with a revert commit on `main`; it deploys the same way.
A release whose pod never gets ready leaves Mneme down, since one pod holds the volume: Flux applies the revert once that release's 5-minute health check has timed out.
Never retag the artifact by hand: Flux accepts only what the release workflow signed on `main`.

## Check it

```sh
mise run gate
```

The gate checks formatting, runs clippy with warnings denied, runs the tests with nextest, checks the RustSec advisories, and lints the CI workflow. Its tests include a simulation of three clones and a server over 128 seeds. `mise run soak` runs 2000. CI runs the same command. Set `MISE_OFFLINE=1` to check the advisories against the local copy of the database without fetching it.
