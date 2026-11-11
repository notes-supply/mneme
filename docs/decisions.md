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
- **Git checks every pushed object** (`receive.fsckObjects`). It refuses tree entries named `.` or `..`, and every name that HFS+ or NTFS reads as `.git` (`.GIT`, `git~1`), before the push check runs. The push check's own `.git` comparison stays as a second line.

## Machine status

- **`seen` counts Git requests only.** A report leaves it alone, so a service that still reports but no longer syncs shows its last sync.
- **A machine also has `reported`,** the time its last report arrived, which the brief did not list. A blocked service makes no Git requests, so without it a blocked service that still reports would look like a stopped one.
- **A report is checked at the door.** `state` is 1 to 64 bytes, `detail` at most 1 KiB, `head` a full lowercase commit id or `null`, and no other field. A body that is not JSON is `400`, one of the wrong shape or with a bad value `422`, and one over 4 KiB `413`, as axum answers them.

## Autosave

- **Local commits are replayed with `git merge-tree` and `git commit-tree`, not `git rebase`.** This departs from the brief, and the reviewer accepted it on this evidence. A rebase walks the work tree through every step and stops on a conflict with markers in the page, so a writer can read or edit a half-rebased folder. Its recovery after a kill, `git rebase --abort`, resets tracked files and throws away an edit made while it was stopped. With Git 2.56: commit `one/two/three` to `p.md` and `other` to `q.md`; on a branch `up` change line two to `UP`; on `main` change it to `LOCAL`; `git rebase up` stops with `UU p.md`; write `agent edit while stopped` to `q.md`; `git rebase --abort` succeeds, and `q.md` reads `other` again. The edit is gone without a word. The replay writes only objects. The folder moves once, with `git reset --keep`, which refuses rather than overwrite an unsaved edit or an untracked file, and only when `HEAD` is still the commit the replay started from. So there is no rebase state, no marker file and nothing to abort, and every tick derives its work from the repository as it finds it.
- **Where a path overlaps, the replayed commit's entry wins**: Git's stage 3 for that path, or no file when that commit has none. A local edit stays, a local delete stays a delete, and a local rename wins. The commit gains `Overlap: <commit>`, the newest commit on the other side that changed one of the overlapping paths, which is the edit that lost. A commit whose replay changes nothing is left out.
- **Git runs without the user's global and system config**, with `core.excludesFile` and `core.attributesFile` at `/dev/null`, no credential helper, no signing, no prompt and none of the caller's `GIT_*` variables. A global gitignore would otherwise keep new pages such as `AGENTS.md` from ever being saved, and a signing key or credential helper can wait for a person. Ignores belong in the wiki's own `.gitignore`. The token is given as `http.<server>.extraHeader` through `GIT_CONFIG_*`, so Git sends it only to the server.
- **The mass-delete stop counts pages lost**: deleted pages less new pages, before anything is staged. Moving every page into a folder loses none. A wiped folder loses all of them. Nothing is committed or staged while it holds, and fetch and push go on so earlier saves still reach the server.
- **A fetch happens when the head moved since the last one**, which covers "right after a save" and hand commits, **after a push found the server had moved**, and otherwise once per interval. Integration runs only after such a fetch, so a replay is never built on a stale view of the server.
- **The service stores file stats in the index before it moves the folder.** Its `git status` runs without optional locks, so that it never takes the index's lock from under a writer, which also means it never stores what it learns. `git merge --ff-only` and `git reset --keep` refuse to move a file whose index entry is out of date even when its text is unchanged, so both run after `git update-index -q --refresh`, which writes only the index. The simulation found this when an index rebuilt from `HEAD`, which holds no stats, met a replay on the same tick.
- **Every failure is an outcome.** A fetch or push that cannot reach the server, or that the server answers with 401, is `offline` with Git's message. An unexpected Git failure is `blocked` with Git's message and is tried again on the next tick. The service exits only on SIGTERM or Ctrl-C.
- **Whether a refused push lost a race is read from the server, not from Git's words.** After any refused push the service asks for the server's `main` (`git ls-remote`). When it is no longer what the fetch saw, the push lost a race and the next tick fetches and replays. Git words two such races differently: `[rejected] (fetch first)` when it notices before sending, and `[remote rejected] (failed to update ref)` when the server only notices as it takes the ref's lock, which reads like a hook's refusal. Two services saving the same page at once hit the second, which a test now makes on purpose. When the server's `main` has not moved, the push check or Git's own object check refused it, and the push is `blocked` with the server's message and not tried again until the head changes.
- **Autosave commits are by `<machine> <autosave@machine>`**, dated by the tick's clock. Replayed commits keep their author, author time and message; their committer is the machine.
- **Machine status reports carry the tick's outcome** as the state, its reason as the detail, and the folder's head, or `null` when a guard stopped the tick before reading it.
- **The quiet time is measured by modification time against the tick's clock.** A deleted file counts as quiet. A file dated in the future waits until the clock passes it.
- **The kill test kills the service's process alone**, as `kill -9` or a crash would. Git commands it started run on to their end. No tick may be `blocked` or `stopped`. An earlier version allowed one `blocked` tick, read as the restarted service meeting such a command's ref update; it was the push race above, and with that fixed, ten seeds and about 270 kills show none. The unit uses `KillMode=mixed`, so a stop sends SIGTERM to the service alone and it finishes its tick.

### The simulation's oracle

The simulation records every commit a clone makes, from the `commit` entries in the reflog of its `main`, which are both the service's saves and hand commits, and leaves out merges, which the server cannot hold. A commit's key is its author name, author time and subject. At the end, after every clone has gone online and ticked until nothing changed, the commits on the server's `main` after the starting commit must meet all of this:

1. No key appears twice, and every key belongs to a recorded commit.
2. Every recorded commit's key appears exactly once, with the same author email and a message that starts with the recorded message. The one exception is a commit whose every change deletes a page that the server's history also deletes: replaying it found nothing left to do.
3. No commit is empty.
4. For each recorded commit `C` found as `S`, Git's three-way merge of `C` onto `S`'s parent, with `C`'s parent as the base (`git merge-tree --merge-base=C^ S^ C`), decides the rest. When it is clean, `S` is exactly that merge and has no new `Overlap` trailer. When it conflicts, `S` has one new `Overlap` trailer and every conflicted path in `S` holds `C`'s entry, or no file where `C` has none.

Every edit the simulation writes carries a token used once, and every page it creates or renames gets a new path, so a change other than a deletion can never already be on `main`. That is why "unless its change was already there" can be checked as deletions only. Each commit is replayed at most once in the simulation, because ticks run one at a time and a replay always follows a fetch in the same tick, so `S`'s parent is the commit it was replayed onto.

Five kinds of overlap are also made on purpose, on a page both clones hold as the server has it: the same line changed, the same new page created, an edit against a delete either way round, and one page renamed two ways. Both clones save before either syncs, and the simulation checks at once that the second clone's tick pushes and that the server's head has one `Overlap` trailer and that clone's side: its text, its delete, or its name.

The gate runs 128 seeds of 80 steps, about a minute here. `mise run soak` runs 2000 seeds and the kill test over 20 seeds.

### A stale `index.lock`

A Git command killed with SIGKILL, by a power cut or by killing the whole unit, leaves `.git/index.lock` behind, and every later command that needs the index fails with `File exists`. Reproduced with Git 2.56: SIGKILL a `git update-index --index-info` that holds the lock while it waits for input, and the next `git add` fails.

- **A lock is stale when its modification time is more than `--stale-lock` seconds old by the tick's clock, 300 by default.** The service then removes it, logs `stale lock removed` with its age, and carries on in the same tick. Until then the tick is `blocked` with the lock's age in the reason. No process is consulted. A command that is still writing the index keeps the lock's modification time fresh, and with Git 2.56 neither `git commit` with its editor open nor `git add -p` waiting for an answer holds `index.lock` at all, so a lock that sits unchanged for minutes belongs to a command that died. With pages capped at 1 MiB by the push check, no honest command in a wiki folder writes the index for that long.
- **A check of `/proc` was tried and dropped.** The rule first asked for no live `git` process to own the lock, judged from `/proc`. Inside the hardened user unit the check is blind: a systemd user unit with any option that needs a mount namespace (`ProtectSystem`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `ProtectKernelTunables`) runs in a user namespace of its own (`uid_map` is `1001 1001 1`), and from there `/proc/<pid>/cwd` and `/proc/<pid>/exe` of the user's other processes cannot be read, though `comm` and `cmdline` can. The sandbox was kept and the check dropped.
- **An index the dead command left unreadable is read again from `HEAD`.** After removing a stale lock the service checks that `git status` works. When it does not, it runs `git read-tree HEAD`, which writes only the index, so every edit in the folder stays and shows as a change to save. When that fails too, the tick is `blocked` with Git's message.
- **The one remaining risk:** a command that holds the lock for more than five minutes without writing to it loses its index update, and its work tree is untouched.

The simulation leaves such a lock, half the time with half an index, and the kill test leaves one while a service is down. Both require that no tick is ever blocked.
