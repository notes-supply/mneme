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

The tokens file maps each machine's name to the SHA-256 of its token. The server creates the repository when it is missing, and installs the push check (`mneme hook pre-receive`) into it. It needs `git` on `PATH`. Logs are JSON lines on standard error. SIGTERM stops it.

The reading API is described in [docs/api.md](docs/api.md).

## Check it

```sh
mise run gate
```

The gate checks formatting, runs clippy with warnings denied, runs the tests with nextest, checks the RustSec advisories, and lints the CI workflow. CI runs the same command. Set `MISE_OFFLINE=1` to check the advisories against the local copy of the database without fetching it.
