# Reading API

Every reading route is `GET`, lives under `/v1`, and answers with JSON. By default there is no authentication at this layer; the network keeps it private. `mneme serve --repo` also has [editing routes](#editing), which commit a change to `main` and always ask for a token that may write. The machines' reports, which `/v1/machines` lists, arrive on the Git routes with a machine's token, as described [at the end](#reports).

`mneme serve --repo` with `--private-api` asks every `/v1` route for `Authorization: Bearer <token>`, with any token from the tokens file, read-only or not. A request without one, or with one the server does not know, is `401` with `WWW-Authenticate: Bearer` and no body, as on the Git routes, whatever route it names. `/healthz` stays open.

`mneme serve --folder DIR` serves the same routes for a clone on the same machine, from `main` in `DIR/.git`, which it follows. It listens on a loopback address only, `127.0.0.1:8081` unless `--addr` says otherwise, and answers `421` with `{ "error" }` to any request whose `Host` is not `localhost`, `127.0.0.1` or `[::1]`, with any port. There `/v1/machines` is always empty, and there are no Git routes.

Each route has one example response in `tests/fixtures/api/`. The tests check every real response against the shape of its example, both ways, so the frontend can test against the same files. To write them again from a real server, run `MNEME_WRITE_EXAMPLES=1 cargo nextest run every_route`.

## Conventions

- **Times** are author times, in seconds since the Unix epoch.
- **Commits** are full 40-character ids of commits on `main`.
- **Page ids** are 16 hexadecimal characters. A page keeps its id across renames. A page created again at the path of a deleted one has a new id.
- **Versions.** A version is a commit that changed a page's text. Events also include renames that keep the text and deletions. Those have `"version": false`.
- **Paging.** `limit` defaults to 50 and is capped at 500. `offset` defaults to 0. A paged answer has `total`, the number of items across all pages.
- **ETag.** Every response of a reading route has a strong `ETag`, the hash of its body. A request with a matching `If-None-Match` gets `304` with no body.
- **Errors** are `{ "error": "..." }`: `400` for parameters that do not parse and for a search that cannot be read, `404` for an unknown route, page, commit or path, or a page with no text at a commit.

## `GET /v1/status`

The head commit and its time, the number of live pages and of commits, and how long the first build of the index took (`build_ms`).

Example: [`status.json`](../tests/fixtures/api/status.json).

## `GET /v1/pages`

Live pages, as `{ "total", "pages": [Page] }`.

| Parameter | Meaning |
| --- | --- |
| `sort` | `title` (the default), `updated`, `created`, `versions`, `words` or `churn` (words added plus removed over all versions) |
| `order` | `asc` or `desc`. The default is `asc` for `title` and `desc` for the rest |
| `limit`, `offset` | paging |
| `deleted=true` | include deleted pages |
| `changed_within_days`, `created_within_days` | a version, or the creation, in the last so many days |
| `min_versions`, `max_versions`, `min_words`, `max_words` | bounds, inclusive |

A `Page` has `id`, `path`, `title`, `words`, `created`, `updated` (the last version), `versions`, `churn`, `questions` (how many [open questions](#get-v1questions) it has now) and `deleted` (the time of the deletion, or `null`). The title is the first level-1 heading, or the file name without `.md`. Words are runs of non-whitespace in the Markdown.

Example: [`pages.json`](../tests/fixtures/api/pages.json).

## `GET /v1/pages/{id}`

`{ "page": Page, "commit", "markdown", "windows" }`. `commit` is the commit of the page's newest event, of any kind, which a change through the [editing routes](#editing) names as its `base`. `markdown` is the current text, or `null` for a deleted page. `windows` has four entries, for the last 7, 30 and 90 days and for all time (`"days": null`). Each one counts `versions`, `sessions` (runs of versions with gaps under 30 minutes), and words `added` and `removed` by those versions.

Example: [`page.json`](../tests/fixtures/api/page.json).

## `GET /v1/pages/{id}/versions`

The page's events, newest first, as `{ "total", "events": [Event] }`. Parameters are `limit`, `offset`, `since` (a time; only events at or after it) and `before` (a time; only events older than it). Both together give a window.

An `Event` has `commit`, `page`, `path` (after the event), `time`, `author` (the commit's author name), `kind` (`created`, `edited`, `renamed` or `deleted`), `from` (the old path, only for `renamed`), `version`, `words` (after the event), `added`, `removed`, `overlap` and `overlap_author`. A rename that also changes the text is one `renamed` event with `"version": true`. A deletion removes all the page's words. `overlap` is the value of an `Overlap:` trailer in the commit message, which the autosave service writes when it had to pick a side, or `null`. It names the commit of the edit that lost. `overlap_author` is that commit's author name, or `null` when `overlap` is `null` or names no commit on `main`.

Example: [`versions.json`](../tests/fixtures/api/versions.json).

## `GET /v1/pages/{id}/versions/{commit}`

The page's text at that commit, as `{ "page", "commit", "markdown" }`. `commit` is the commit whose event left the text, which is at or before the one asked for. `404` when the page did not exist yet, or was deleted.

Example: [`version.json`](../tests/fixtures/api/version.json).

## `GET /v1/pages/{id}/diff`

The changed lines between two texts, as `{ "from", "to", "lines" }`.

| Parameter | Meaning |
| --- | --- |
| `to` | a commit. The default is the current text |
| `from` | a commit. The default is the text just before the last change at or before `to` |

`from` and `to` in the answer are the commits whose events left the two texts. `from` is `null` when the page did not exist before. A deleted page's text is empty, so the default diff of a deleted page removes all of it.

Each line is `{ "kind", "value" }`: `same` with the line, `removed` or `added` with a list of spans `{ "text", "changed" }` where `changed` marks the changed words, or `skipped` with the number of unchanged lines left out. `lines` is `null` when either text has more than 20,000 lines.

Example: [`diff.json`](../tests/fixtures/api/diff.json).

## `GET /v1/changes`

Events across all pages, newest first, as `{ "total", "events": [Event] }`. Parameters are `limit`, `offset`, `since` and `before`, as for a page's versions. A client that pages back through time asks for `before` the oldest `time` it has. Several events can share one time, so it then drops the events it already holds.

Example: [`changes.json`](../tests/fixtures/api/changes.json).

## `GET /v1/questions`

The open questions of live pages, as `{ "total", "questions": [Question] }`: the most recently updated page first, and each page's questions in the order they appear on it. Parameters are `limit` and `offset`.

A writer leaves a question as a list item under a heading named "Open questions", at any level, compared without case or surrounding space. Each top-level item between that heading and the next heading of the same or a higher level is one question. A nested list is part of its item's text, and an item with no text is left out. A ticked task item (`- [x]`) is an answered question and is left out too. An unticked one (`- [ ]`) is open.

A `Question` has `page`, which is `{ "id", "path", "title" }`, `text`, the item's visible text on one line, and `updated`, the page's `updated`. A page with very much markup, which search reads as raw Markdown, has no questions.

Example: [`questions.json`](../tests/fixtures/api/questions.json).

## `GET /v1/paths/{path}`

The page for a path, as `{ "id", "path", "current" }`. The path may hold slashes and must match exactly. When a live page is at the path, that is the page and `current` is `true`. Otherwise it is the page that most recently had the path and is not deleted, with the path it has now and `"current": false`, so a link written before a rename still finds its page. `404` when no page that is not deleted has had the path.

Example: [`path.json`](../tests/fixtures/api/path.json).

## `GET /v1/search?q=`

Pages whose title or visible text match, as `{ "total", "hits": [Hit] }`. A `Hit` is `{ "page": Page, "score", "snippet" }`. A higher `score` is better; it is FTS5's `bm25` with the title weighing three times the text. `snippet` is a list of `{ "text", "hit" }` runs, with `hit` set on the matched words.

`q` is words separated by spaces, all of which must match:

- `word` matches the word, without case or diacritics (`cafe` finds `café`).
- `"a phrase"` matches the words in that order.
- `word*` matches words that start with `word`.
- `-word` or `-"a phrase"` leaves out pages that match it.

Anything else in a word is searched as text, never read as FTS5 syntax. A search with no words, only exclusions, an open quote, a quote inside a word, or a `*` that does not end a word is `400`.

Visible text is what a reader sees: link text but not link targets, code, and no Markdown markup. A page with very much markup is searched as its raw Markdown.

`sort`, `order`, `limit`, `offset` and the filters of `/v1/pages` apply as well. The default sort is by `score`, best first.

Example: [`search.json`](../tests/fixtures/api/search.json).

## `GET /v1/machines`

Every machine in the tokens file, by name, as `{ "total", "machines": [Machine] }`. The server keeps this in `mneme/machines.json` inside the bare repository, so it survives a restart. A change is written within 3 seconds and at a clean stop, so a crash loses at most the last few seconds.

A `Machine` has `name`, `seen` (when it last made a Git request with its token that carried no report, so its last fetch or push), `reported` (when its last report arrived), the `state`, `detail`, `head`, `skipped` and `skipped_more` of that report, `current`, which is `true` when the reported head is the server's head, and `behind_since`, the server's time when the two heads first differed. Every field but `name` and `current` is `null` until the machine has done that, and `behind_since` is `null` again while the heads are the same.

Example: [`machines.json`](../tests/fixtures/api/machines.json).

## Editing

`mneme serve --repo` also takes changes to pages, for clients that are not a folder of files, such as a web editor or an agent. Each route commits one change to `main`. It asks for `Authorization: Bearer <token>` with a token that may write, with or without `--private-api`: without a token the server knows it is `401` with no body, and with a read-only one `403` with `{ "error" }`. A body is JSON with `Content-Type: application/json` and at most 8 MiB, and has only the fields listed. One that is not JSON is `400`, one without that type `415`, one too long `413`, and one with a field missing, unknown or of the wrong type `422`, each with `{ "error" }`.

A change names its `base`: the `commit` of the newest event of the page it started from, as `GET /v1/pages/{id}` gives it, or the first event of `/v1/pages/{id}/versions`. When the live page at the change's path has had another event since, because someone changed, moved or deleted it, the change is `409` with `{ "error", "current" }` and nothing changes. `current` is the commit of that page's newest event now, or `null` when no live page is at the path. Only the page's own history counts: other pages may have moved on, and the change is made on top of `main` as it is then.

`author` is `{ "name", "email" }`. Each is one line, not blank, with no `<`, `>` or NUL. The commit's author is that, its committer is the token's machine as `<machine> <api@machine>`, and both are dated by the server's clock. `message` may be left out or `null`, and is then the default each route names. It is at most 64 KiB, with no NUL.

A change keeps the wiki's rules, as a push must. A page's path ends in `.md`, is relative, and has no empty, `.` or `..` part, no NUL, no part named `.git` in any case and no name Git will not store, such as `git~1`. No file or folder may be in its way, and a page is at most 1 MiB. A change that breaks one is `422` with `{ "error" }`, and so is a bad `author` or `message`, or a `base` that is not a full commit id.

The answer is read once the index has taken the commit, so a read that follows sees it, and `/v1/machines` compares every machine with the new head as after a push. Pushes go on meanwhile. When one lands while a change is made, the change is checked against the new `main` and made again on top of it. After four such rounds it is `503` with `{ "error" }` and `Retry-After: 1`.

### `PUT /v1/paths/{path}`

Writes the page at `path` with `{ "markdown", "base", "author", "message" }`. `base` must be there, and is `null` to create a page, so that a live page at the path is a conflict. The answer is `201` for a new page and `200` otherwise, with `{ "commit", "page": Page }`. `commit` is the base for the next change: the new commit, or the page's newest event when the text was already the page's, since no commit is made then. The default message is `Create <path>` or `Edit <path>`.

Example: [`edit.json`](../tests/fixtures/api/edit.json).

### `DELETE /v1/paths/{path}`

Deletes the page at `path`, with `{ "base", "author", "message" }`, where `base` is a commit. The answer is `{ "commit" }`. The default message is `Delete <path>`.

Example: [`deletion.json`](../tests/fixtures/api/deletion.json).

### `POST /v1/moves`

Moves the page at `from` to `to`, with `{ "from", "to", "base", "author", "message" }`. The page keeps its text and its id. `base` is about the page at `from`, and a live page at `to` is a conflict too, whose `current` is still that of the page at `from`. The answer is `{ "commit", "page": Page }`. The default message is `Move <from> to <to>`.

Example: [`move.json`](../tests/fixtures/api/move.json). A conflict: [`conflict.json`](../tests/fixtures/api/conflict.json).

## Reports

An autosave service reports its state in a `Mneme-Report` header on a Git request, which it makes with `git ls-remote` and its token. Git sends it, so the report takes the same path and trusts the same certificates as syncing. The header's value is the report's JSON in base64, `{ "state", "detail", "head", "skipped", "skipped_more" }`. `state` is a short word such as `idle` or `blocked`, `detail` says why or is empty, and `head` is the clone's `main` as a full commit id, or `null` when the folder could not be read. `skipped` lists, by path, up to 20 files the service leaves in the folder and out of every save, each as `{ "path", "reason" }`, and `skipped_more` counts the rest. A long list is cut shorter so that the header stays under 4 KiB. The reasons are:

| `reason` | The file |
| --- | --- |
| `a symlink` | is a symbolic link |
| `a submodule or nested repository` | is a folder with a Git repository of its own; its path ends in `/` |
| `a path named .git` | has `.git` in any case as one of its path's parts |
| `a page over 1 MiB` | is a `.md` file over 1 MiB |
| `a file over 10 MiB` | is any other file over 10 MiB |
| `a name Git will not store` | is new and has a name Git refuses, such as `git~1`, which Windows reads as `.git` |

The server stores the report and answers the Git request as usual. A request that carries a report does not change `seen`. A header over 4 KiB is `413`, one that is not base64 or not JSON is `400`, and a report with missing or unknown fields or bad values is `422`, each with `{ "error" }` and without running the Git request. A request without the right token is `401` with no body, whatever it carries.

A machine whose token is read-only (`"access": "read"` in the tokens file) may fetch and report like any other. A push with it is refused before Git runs: `info/refs?service=git-receive-pack` and `git-receive-pack` answer `403` with a one-line `text/plain` reason, which Git shows to the user, and store nothing.

The autosave service sends these states. The detail is empty unless the list says otherwise.

| `state` | Meaning |
| --- | --- |
| `idle` | nothing to do |
| `saved` | it committed the folder's changes |
| `pulled` | it brought in changes from the server |
| `pushed` | the server has its commits |
| `offline` | it cannot reach the server; `detail` is Git's error |
| `blocked` | it waits on the folder and changes nothing; `detail` says why |
| `stopped` | it refused a save that would delete most pages; `detail` says how many |

A service that follows (`--follow`) never saves, so it sends only `idle`, `pulled`, `offline` and `blocked`, and no skipped files.

Example report, before encoding: [`report.json`](../tests/fixtures/api/report.json).
