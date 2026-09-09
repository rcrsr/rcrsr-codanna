# Release SOP — rcrsr-codanna

Standard operating procedure for cutting a release of this fork. This file is
the source of truth for `/conduct:cut-release` and for anyone releasing by
hand. Where it is more specific than a generic release procedure, it wins.

Related reading: `CLAUDE.md` § Fork discipline, `RCRSR-README.md`
§ Installing the fork, the header comment in `Cargo.toml`, and
`.github/workflows/release.yml`.

## 1. Version format and tag scheme

- The version is the **full** string `<upstream>+rcrsr.N`, e.g. `0.16.0+rcrsr.4`.
  Never release a bare `<upstream>` version and never drop the `+rcrsr.N`
  suffix. `+rcrsr.N` is semver **build metadata**: it compares *equal* to the
  bare upstream version and is not orderable.
- `<upstream>` moves only when the fork is rebased onto a new upstream
  release. `N` is monotonic across the fork's whole life: it only ever
  increments, never resets, and moving the base does not touch it.
- The release argument to `/conduct:cut-release` is the full version, with the
  literal `+`: `/conduct:cut-release 0.16.0+rcrsr.4`.
- **Tag:** `v<version>` with the `+` intact, e.g. `v0.16.0+rcrsr.4`. CI
  (`release.yml`, "Extract version") hard-fails if the pushed tag is not
  exactly `v` + the `Cargo.toml` version. Asset filenames use a sanitized
  `+` → `-` form; the tag never does.
- No qualifier / sub-package releases. This is a single-crate repository;
  the `{qualifier}` argument does not apply.

## 2. Version source of truth

The version lives in **three places that must agree**, all in the repo root:

| Location | What to set |
|----------|-------------|
| `Cargo.toml` → `[package] version` | `<version>` (raw, with `+`) |
| `Cargo.toml` → `[package.metadata.binstall.overrides.*]` — four `pkg-url` filename segments and four `bin-dir` values | the **sanitized** version, `+` → `-` (e.g. `0.16.0-rcrsr.4`). The `download/v{ version }/` path segment keeps the raw `{ version }` template and must not be hand-edited. |
| `Cargo.lock` → the `codanna` package entry | regenerated, never hand-edited: run `cargo check` (or `cargo build`, optionally with `--offline`) after editing `Cargo.toml` — avoid `cargo update`, which is a dependency-upgrade command and can pull in newer transitive dependency versions beyond the version edit |

`tests/binstall_metadata_tests.rs` guards the binstall literals against drift
from `Cargo.toml`'s version. Run it after any bump:

```bash
cargo test --test binstall_metadata_tests
```

**Bump timing.** In this fork the version is normally bumped in the feature or
upstream-advance PR that earns it (a base move or a new fork-private
addition), not in the release PR. So at release time first check:

```bash
grep -m1 '^version = ' Cargo.toml
```

- If it already equals `<version>`: **no bump**; the release PR is
  changelog-only (this is the common case — see PR #72 for the pattern).
- If it does not: bump all three locations above in the release PR, run the
  binstall test, and make sure `CHANGELOG.md`'s `[Unreleased]` section
  explains why `N` (or the base) moved.

There is no sync script. `publish = false` is set on purpose — the `codanna`
crate name is owned by upstream. **Never run `cargo publish`.**

## 3. Changelog

Single changelog: `CHANGELOG.md` (root). Keep a Changelog format, bracketed
headings.

Stamp it as follows:

1. Rename `## [Unreleased]` to `## [<version>] - <YYYY-MM-DD>` using the
   **full** version with the literal `+`, e.g. `## [0.16.0+rcrsr.4] - 2026-09-08`.
   The release body is extracted by `contributing/scripts/changelog-section.sh`
   with an exact string match on the bracket contents, so `## [0.16.0]` or a
   sanitized `0.16.0-rcrsr.4` will not be found.
2. Insert a fresh, empty `## [Unreleased]` immediately above the new dated
   heading.
3. **Ordering.** Entries are version-descending, and because `+rcrsr.N`
   compares equal to its bare upstream base, a fork entry sits **directly
   above the bare upstream entry it is built on** — not above newer upstream
   bases. In practice: `[Unreleased]` is already positioned directly above the
   current base's section (e.g. `## [0.16.0]`), so renaming it in place is
   correct. If newer upstream sections have been copied in above it, move the
   stamped section down to sit right above its own base.
4. **Do not add a bottom link-reference line** for the fork version. The
   `[x.y.z]: https://github.com/bartolli/codanna/compare/...` block at the
   bottom is upstream's and covers only bare upstream versions; fork entries
   have never carried one (see `[0.13.1+rcrsr.3]`).

The section must have a non-empty body. `release.yml` validates this in its
`info` job and fails the tag push in seconds if the heading is missing or
empty. Verify locally before opening the PR:

```bash
contributing/scripts/changelog-section.sh <version>
```

## 4. Other files to touch in the release PR

- `CLAUDE.md` § Fork discipline, first bullet: update the sentence
  "The last released fork tag is `v…`; the current in-flight version is
  `…`" so the released tag is the one being cut and the in-flight version is
  the next expected one (usually the same base with `N+1`, or "none yet").
- `RCRSR-README.md` § Installing the fork: only if the user-facing install
  instructions or examples changed. The pinned-version examples
  (`v0.12.0+rcrsr.1`) are illustrative and do not need to track the release.
- **Deprecated-for-one-release-cycle flags:** if a prior `[Unreleased]` entry
  deprecated a flag "for one release cycle" (e.g. `codanna serve --list` in
  favor of `codanna ls`), this release is that cycle's boundary. Check
  whether the deprecation has now spanned a full release; if so, file (or
  fold into this PR) the follow-up to remove the deprecated flag rather than
  letting it linger silently past its stated grace period. Each such
  deprecation carries a machine-checkable `// TODO(remove-after: vX.Y.Z)`
  comment next to the flag's definition (e.g. `list: bool` in
  `src/cli/args.rs`); grep for `TODO(remove-after:` and compare the version
  against the one being cut here instead of relying on memory alone.

## 5. Branch, commit, and PR conventions

- **Branch:** `release/<version>` (e.g. `release/0.16.0+rcrsr.4`). Git accepts
  `+` in ref names.
- **Commit:** `chore(release): prepare <version>`. Stage only the files this
  procedure edits (`CHANGELOG.md`, `CLAUDE.md`, and — on a bump —
  `Cargo.toml`, `Cargo.lock`). Never `git add .`.
- **PR title:** `chore(release): prepare <version>`. No `[area]` prefix — the
  `Label` workflow (`.github/workflows/label.yml`) applies `area:docs` /
  `area:dx` automatically per `.github/labeler.yml`'s path mappings.
- **PR body:** lead with a prose narrative of the release, then a
  `## What ships` summary and a `## Release note` reminding that tagging is a
  separate step after merge. Point at the stamped CHANGELOG section for full
  detail rather than duplicating it.
- **Merge:** squash, subject `chore(release): prepare <version> (#<pr>)`. CI
  must be `CLEAN` with zero unresolved review threads (Quick Checks, Full Test
  Suite, Apply area labels).

## 6. Pre-tag verification

Run from the repo root on the merged default-branch tip **before** pushing the
tag. All must pass:

```bash
cargo test --test binstall_metadata_tests           # version literals agree
contributing/scripts/changelog-section.sh <version> # section exists, non-empty
./contributing/scripts/full-test.sh                 # mirrors CI's Full Test Suite
contributing/scripts/test-release-workflow.sh       # local mirror of release.yml:
                                                    # version derivation, release
                                                    # build, packaging, checksums,
                                                    # manifest, offline install.sh
```

`test-release-workflow.sh` takes several minutes (it does a full
`--release --all-features` build). `workflow_dispatch` on `release.yml` is
also available as a remote dry run: it builds and packages but **never**
publishes a release, even when dispatched against a tag.

## 7. Tag and publish

**Pushing the tag is the publish step.** `release.yml` triggers on
`push: tags: v*` and, for a real tag push, runs the full test suite, builds
all four platforms (linux-x64, macos-x64, macos-arm64, windows-x64), packages
archives + sha256/sha512 checksums + `dist-manifest.json`, and creates the
GitHub Release itself via `softprops/action-gh-release` with the body taken
from the matching `CHANGELOG.md` section. It does **not** deploy anywhere.

```bash
git checkout main && git pull --ff-only
git tag -a v<version> -m "Release <version>"
git push origin v<version>
```

- Use an annotated tag; the message may carry a short summary (see
  `git tag -n5 v0.13.1+rcrsr.3` for the style).
- **Do not run `gh release create`.** The workflow owns the release; a
  manually created release would collide with it or leave one without
  assets. If the workflow fails after the tag is pushed, fix the cause, delete
  the tag locally and remotely, and re-push — do not create the release by
  hand.
- Watch the run: `gh run watch` or `gh run list --workflow=release.yml`.
  Expect ~20–30 minutes. When it finishes, confirm with
  `gh release view v<version>` that the release is published (not draft)
  and lists `dist-manifest.json`, four archives, `SHA256SUMS`, `SHA512SUMS`,
  and the per-archive checksum files.

## 8. Post-release checks

```bash
# Installer resolves the new release and its checksum verifies
curl -fsSL https://raw.githubusercontent.com/rcrsr/rcrsr-codanna/main/scripts/install.sh | sh
codanna --version                         # prints <version> with +rcrsr.N

# binstall metadata resolves the sanitized asset names
cargo binstall --git https://github.com/rcrsr/rcrsr-codanna codanna --dry-run
```

Then open a follow-up (or fold into the next feature PR) to keep the
in-flight version pointer in `CLAUDE.md` current, and start the next
`[Unreleased]` section as work lands.

## 9. Mapping to `/conduct:cut-release` phases

| Phase | This repo |
|-------|-----------|
| 1 Arguments | version is the full `<upstream>+rcrsr.N`; no qualifier |
| 2 Conventions | this file; tag-trigger detection will report `release.yml` as `[auto-generates release notes]` — correct, and it does not deploy |
| 3 Branch | `release/<version>` |
| 4 Bump | usually a no-op (already bumped upstream of the release); otherwise `Cargo.toml` (version + 8 binstall literals) and regenerate `Cargo.lock`; run `cargo test --test binstall_metadata_tests` |
| 5 Changelog | `CHANGELOG.md` only; full version in the heading; fork-ordering rule; no bottom link-ref; verify with `changelog-section.sh` |
| 6 PR | also edit `CLAUDE.md`'s released/in-flight pointer; commit and title `chore(release): prepare <version>` |
| 7 Wait | standard merge-readiness (CLEAN, 0 threads, no pending checks) |
| 8.1 Merge | squash, subject `chore(release): prepare <version> (#<pr>)` |
| 8.2 Tag | `v<version>` with `+`, annotated |
| 8.3 Release | **skip** — `release.yml` publishes it; report the tag push and the workflow run URL instead |
