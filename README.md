# Mneme

Mneme keeps a Markdown wiki in Git, so that the people and agents who write it only edit files. A small service on each writer's machine commits what has been still for 15 seconds, syncs with the other machines through one Git remote, and pushes, without ever stopping to ask anyone anything. The remote can be a repository on a Git host, a bare repository on the same machine, or a Mneme server, which adds a check on every push, a token per machine, each machine's state and a reading API. [Atrium](https://github.com/notes-supply/atrium) shows the wiki in a browser.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/terminal-dark.png">
  <img alt="A terminal. On the laptop, mneme init clones the wiki from the server, installs the autosave service and starts it. On the desktop, a line is added to garden.md, and half a minute later the laptop's log shows the desktop's autosave commit and the page has the line." src="docs/images/terminal-light.png">
</picture>

## Install

Mneme runs on Linux, on x86_64 and aarch64, and needs Git 2.45 or newer. This installs `mneme` and `atrium` into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/notes-supply/mneme/main/install.sh | sh
```

It checks each binary against its release's `SHA256SUMS`, and against its signature when [cosign](https://docs.sigstore.dev/cosign/system_config/installation/) is on `PATH`, and installs Atrium's user unit without enabling it. `sh -s -- mneme` installs Mneme alone, and `MNEME_INSTALL_DIR` names another folder.

Or with [mise](https://mise.jdx.dev):

```sh
mise use -g github:notes-supply/mneme github:notes-supply/atrium
```

mise keeps each version in a folder of its own, and the service runs Mneme by its full path, so run `mneme init --force` with the same options after an upgrade.

## Quick start

`mneme init` makes `~/wiki` ready, installs the autosave service as a systemd user unit, and starts it. Run it once on each machine, with the same remote. Each machine's commits carry its name, the short host name unless `--name` gives another. A missing or empty folder gets a clone, and a folder of notes becomes the wiki when the remote is empty. `--folder` names another folder, and `--no-service` prints the command that runs the service instead of installing it. The service's logs are in `journalctl --user -u mneme-autosave`.

**A Git host over HTTPS.** Make an empty private repository, and for each machine an access token that can read and write that repository's contents and nothing else, such as a fine-grained token on GitHub scoped to the one repository. Save it in a file only you can read, then:

```sh
mneme init --remote https://github.com/you/wiki.git --token-file ~/.config/mneme/token
```

The token is sent as the password of HTTP Basic authentication, to that URL alone. A host that wants the token's user name takes it in the URL, as `https://you@host/wiki.git`.

**A Mneme server.** The server's tokens file names this machine as `--name` does (see [Run a server](#run-a-server)), and the token file holds its token:

```sh
mneme init --server https://mneme.example --token-file ~/.config/mneme/token
```

**A path**, such as a bare repository on this machine:

```sh
git init --bare -b main ~/wiki.git
mneme init --remote ~/wiki.git
```

`--remote` takes these three and nothing else, and plain `http://` only to this machine.

## Read the wiki

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/changes-dark.png">
  <img alt="Atrium's recent changes for the wiki, by laptop and desktop. The newest, a desktop edit of Garden, says laptop edited the same lines." src="docs/images/changes-light.png">
</picture>

Atrium serves the wiki in `~/wiki` at http://localhost:8080, to this machine alone. Start the unit `install.sh` put in place:

```sh
systemctl --user enable --now atrium
```

or run `atrium --wiki-folder ~/wiki` in a terminal. It reads the folder through `mneme serve --folder ~/wiki`, which serves the [reading API](docs/api.md) of the clone, pages, history, diffs and search, on loopback only, and follows the folder's `main` as the service syncs. It writes nothing.

## Run a server

```sh
printf '{ "laptop": "%s" }\n' "$(printf %s "$TOKEN" | sha256sum | cut -d' ' -f1)" > tokens.json
mneme serve --repo /data/wiki.git --tokens tokens.json --addr 0.0.0.0:8080
```

The tokens file maps each machine's name to the SHA-256 of its token. The server creates the repository when it is missing and checks every push against the wiki's rules. Machines sync over `/git/wiki.git` with their tokens and report their state, which `/v1/machines` lists. It needs Git 2.45 or newer with `git http-backend`, listens on `127.0.0.1:8080` unless `--addr` says otherwise, logs JSON lines on standard error, and stops on SIGTERM. The image `ghcr.io/notes-supply/mneme` runs it as UID 65532, and works on a read-only root; [docs/deploy.md](docs/deploy.md) has the image, the releases and a Flux setup that verifies them.

> [!WARNING]
> `/v1` has no login: whoever reaches it reads the whole wiki. Beyond the people who may read all of it, expose only `/git/`. On a tailnet, `tailscale serve` in front of the server is enough.

## How autosave behaves

- A change is saved once every changed file has been still for 15 seconds, then pushed. The service fetches right after, and every 30 seconds.
- When two machines change the same lines, the later one keeps its own text and marks its commit with an `Overlap:` trailer. The other text stays in the history.
- It waits while the folder is in the middle of a Git operation it did not start, and stops saving when a save would delete most pages.
- A file the server would refuse, such as a symlink or a page over 1 MiB, stays in the folder and out of every save, and the service reports it.
- Git runs without your own Git configuration, so ignores belong in the wiki's `.gitignore`.
- A plain remote stores what it is sent: there is no push check, so a commit by hand with such a file gets in, and no `/v1/machines`, so a machine's log is the only place that says it is stuck.

## Verify a download

Each release has a static binary for each architecture, `SHA256SUMS`, and a cosign bundle per binary, signed keyless by the workflow that built it. To check one by hand:

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

Atrium's are checked the same way, with `atrium` in place of `mneme`.

## More

- [docs/api.md](docs/api.md): the reading API.
- [docs/deploy.md](docs/deploy.md): the server's image, releases, and a Flux cluster.
- [docs/development.md](docs/development.md): the checks, building from source, and the screenshots.
- [docs/decisions.md](docs/decisions.md): why Mneme works the way it does.

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Mneme by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
