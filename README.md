# git-extractor-rs

Rust reimplementation of [GitTools](https://github.com/internetwache/GitTools)' Dumper and Extractor. Two binaries:

- `git-dumper` downloads an exposed `.git` directory over HTTP(S), even without directory listing (it only retrieves files, no extraction);
- `git-extractor-rs` extracts every object and a snapshot per commit from a local repository, typically the dump.

```bash
git-dumper https://target.tld/.git/ ./dump
git-extractor-rs ./dump ./xtracted
```

## git-extractor-rs

Rust implementation of a Git object extractor. It uses Git's object database interface rather than parsing `.pack` / `.idx` filenames.

## Requirements

- Rust stable toolchain
- Git available in `PATH` (only `git-extractor-rs` needs it; `git-dumper` does not)

## Build

```bash
cargo build --release
# -> target/release/git-dumper and target/release/git-extractor-rs
```

## Usage

```bash
# Output directory as second argument (same as GitTools' extractor.sh)
./target/release/git-extractor-rs /path/to/repository /tmp/xtracted

# or with the option form
./target/release/git-extractor-rs /path/to/repository -o /tmp/xtracted --threads 8
```

Without an output directory, `./xtracted` is used. The positional form and `-o` cannot be combined.

| Option | Description |
|---|---|
| `[OUTPUT]` / `-o, --output <DIR>` | Destination directory (default `./xtracted`) |
| `-j, --threads <N>` | Parallel `git cat-file --batch` workers (default 4) |
| `--snapshot-date <committer\|author>` | Date used to prefix snapshot directories (default `committer`) |
| `--report <bool>` | Write `report.json` (default `true`) |

The repository argument may be a working tree, a `.git` directory, or a bare repository.

## Output

```text
output/
├── blobs/<object-hash>
├── commits/<object-hash>
├── trees/<object-hash>
├── tags/<object-hash>
├── snapshots/
│   └── <YYYYMMDD-HHMM>_<commit-hash>/
│       └── <original repository paths>
├── metadata/objects.json
├── metadata/snapshots.json
└── report.json
```

Every object is stored by its Git object ID. Blob contents are raw file contents; commit/tree/tag contents are raw Git object payloads.

## Important

`git cat-file --batch-all-objects` enumerates packed and loose objects, including unreachable objects present in the object database. This does not reconstruct original filenames for blobs by itself: a blob can have multiple historical paths, or no path at all. Use the extracted tree and commit objects (or Git itself) to resolve path history.

This tool does not run secret detection. Run Gitleaks and/or TruffleHog separately against the repository, and inspect extracted blobs for unreachable-object findings.


## Commit snapshots

For every commit object found in the object database, the tool runs `git archive` and exports the tracked regular files into:

```text
snapshots/<YYYYMMDD-HHMM>_<commit-hash>/<original-path>
```

The date prefix makes `ls snapshots/` list commits chronologically, including unreachable ones. It is read from the raw commit object and expressed in the commit's own timezone, so it matches what `git log` shows. By default the committer date is used (when the commit was created, rewritten by rebase/amend/cherry-pick); use `--snapshot-date author` for the original authoring date. A commit whose date cannot be parsed is prefixed with `00000000-0000`.

`metadata/snapshots.json` maps each commit to its directory, date, Unix timestamp, timezone offset and number of exported files.

This includes commits not reachable from current branches, provided their commit objects remain in the repository. The snapshot contains the full tree at that commit, not only the files changed by that commit. Symlinks and special filesystem entries are intentionally skipped; the raw Git objects remain available under `blobs/`, `trees/`, etc.


## git-dumper

```bash
git-dumper <URL> <OUTPUT> [options]
git-dumper https://target.tld/.git/ ./dump -j 16
git-dumper https://target.tld/.git/ ./dump -k -x http://127.0.0.1:8080 -H "Cookie: session=..."
```

The repository is written to `<OUTPUT>/.git`, which is a regular Git repository: run `git-extractor-rs <OUTPUT> <dir>`, or `git checkout .` inside `<OUTPUT>` to restore the working tree of HEAD.

| Option | Description |
|---|---|
| `--git-dir <NAME>` | Name of the git directory (default `.git`), as in gitdumper.sh |
| `-j, --threads <N>` | Parallel downloads (default 8) |
| `-H, --header <H>` | Extra header, repeatable (`"Name: value"`) |
| `-A, --user-agent <UA>` | User-Agent (default: a browser one) |
| `-k, --insecure` | Skip TLS certificate verification |
| `-x, --proxy <URL>` | HTTP/HTTPS/SOCKS proxy (Burp, etc.); `HTTPS_PROXY`/`NO_PROXY` are also honored |
| `--follow-redirects` | Follow redirects (off by default: they are usually error pages) |
| `--timeout <S>` | Per-request timeout (default 20) |
| `--strict-url` | Abort if the URL does not end with `/<git-dir>/` (otherwise only a warning) |
| `-v, --verbose` | Also print cached, missing and rejected files |

### How it works

Same discovery approach as `gitdumper.sh`, done breadth-first and in parallel:

1. Fetch well-known paths: `HEAD`, `ORIG_HEAD`, `FETCH_HEAD`, `config`, `index`, `packed-refs`, `objects/info/packs`, `info/refs`, `refs/heads/{master,main}`, `refs/stash`, the matching `logs/...`, the WIP refs, etc.
2. Parse each file downloaded and queue what it references:
   - text files (refs, logs, `packed-refs`, `FETCH_HEAD`, `info/refs`...): object hashes, `refs/...` names (and their reflogs), `pack-<hash>` names;
   - `config`: `[branch "x"]` and `[remote "y"]` sections become refs to try;
   - loose objects: inflated and parsed natively; commits give their tree and parents, tags their target, trees their entries (submodule gitlinks are skipped);
   - `index` (v2 to v4): every staged blob, **including files staged but never committed**;
   - `objects/info/packs`: `.pack` and `.idx` files.
3. Repeat until nothing new is found.

Differences from the original script:

- no dependency on `git`, `curl` or `strings`;
- **every loose object is verified against its SHA-1** and every pack against its trailing checksum. Servers that answer `200` with an HTML page for missing files (soft 404) cannot pollute the dump: the pages are counted as `rejected` and never written;
- refs found anywhere (packed-refs, HEAD, logs, config, info/refs) are followed with their reflogs, not only the hard-coded master/main list;
- re-running the same command resumes: valid files already on disk are reused (`cached`), not downloaded again;
- redirects are not followed by default (`--follow-redirects` to enable); TLS verification is on by default (`-k` to disable, gitdumper.sh always disables it).

### Limits

- Objects stored in packs are only retrieved if the pack name is discoverable, usually through `objects/info/packs` (generated by `git update-server-info`, missing on many servers) or a `pack-<hash>` string in another file. Without it, only loose objects can be reached.
- Unreachable objects that nothing references cannot be guessed over HTTP, unlike with a local repository.
- Only SHA-1 repositories are supported (40-hex object ids).
