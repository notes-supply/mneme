# Decisions

Choices made while building Mneme that the design brief left open or that differ from it.

## Toolchain and gate

- **Nightly 2026-10-28.** The newest dated nightly already installed on the build machine. Move the date in `mise.toml` deliberately.
- **Cargo's `unused_dependencies` lint, denied,** replaces rustc's `unused_crate_dependencies` from the house lint table. The rustc lint misfires on a crate with a library, a binary and integration tests.
- **Advisories fail on every vulnerability.** cargo-deny 0.20 has no severity threshold, so the brief's "High or Critical" became "any". `MISE_OFFLINE=1` checks against the local advisory database instead of fetching it.

## Index and history

- **gix stays at `sha1` and `blob-diff`.** Without gix's `parallel` feature a repository handle cannot cross threads, so each blocking task opens the repository from its path. Opening is cheap, and it means every read sees the newest objects.
- **A page id is 16 hexadecimal characters** of the SHA-256 of the creating commit and the first path.
- **Word counts are of the Markdown**, as in the wiki viewer, so `#` and link targets count as words.
- **A page over the markup budget** has its file name as its title, because finding its heading would need the parse the budget avoids.
- **A deletion is an event with `removed` set to the page's words.** It is not a version, so the windows, which count versions, leave it out.
- **Times are seconds since the Unix epoch.** `since` takes the same.
- **Commits in the API are full ids** of commits on `main`. A commit between two events of a page shows the text the earlier event left.

## API

- **Every listing has a `total`.** The page filters of search also work on `/v1/pages`.
- **Search scores are `-bm25`,** so higher is better and `desc` is the default order like every other numeric sort.
- **Snippets are lists of runs** `{ text, hit }` rather than marked-up strings, so the frontend never parses HTML out of page text.
- **docs/api.md links its examples** in `tests/fixtures/api/` instead of copying them, so the documented responses are the tested ones.

## Git over HTTPS

- **The tokens file is one JSON object** of machine name to hexadecimal SHA-256.
- **A 401 carries `WWW-Authenticate: Bearer`** and no body. HTTP asks for the header, and it says nothing about why.
- **Only the three smart HTTP routes exist.** The dumb protocol is not served.
- **A body over 64 MiB** is refused with 413 when its length is declared, and cut off when it is streamed.
- **The push check also refuses a deletion of `main`,** before `receive.denyDeletes` would, and compares `.git` without case, since clones on macOS and Windows read `.GIT` as `.git`.
