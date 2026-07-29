#!/usr/bin/env bash
# Local mirror of .github/workflows/release.yml -- keep in sync.
#
# Exercises the release pipeline end-to-end on this host: version /
# asset_version derivation (info job), the release-matrix build (build job,
# --all-features only -- there is no slim variant), packaging with the
# sha256sum/shasum fallback idiom, bulk checksums over the narrowed archive
# glob, manifest generation with the real {version, artifacts:[...]} schema
# and prefix-strip platform derivation, CHANGELOG release-notes extraction
# via the shared changelog-section.sh, and an OFFLINE end-to-end run of
# scripts/install.sh against the archive this script just built.
#
# This file does NOT reimplement the Cargo.toml binstall-template drift
# guard (raw `{ version }` vs. the hardcoded sanitized asset_version
# literals) in shell -- that assertion lives in
# `cargo test --test binstall_metadata_tests` and is delegated to, not
# duplicated, below.
#
# If you change .github/workflows/release.yml, mirror the change here in the
# same PR. Every command below has a direct counterpart in release.yml; if a
# step here has no such counterpart, it does not belong in this file.
set -euo pipefail

if [[ ! -f Cargo.toml ]] || [[ ! -f scripts/install.sh ]]; then
  echo "Error: run this script from the repository root (Cargo.toml / scripts/install.sh not found in \$PWD)." >&2
  exit 1
fi

echo "Testing Release Workflow Locally"
echo "================================="

test_dir=$(mktemp -d "${TMPDIR:-/tmp}/codanna-release-test.XXXXXX")
trap 'rm -rf "$test_dir"' EXIT

fail() {
  # Mirrors the "Extract version" step's "Error: ... / expected ... / actual ..." shape:
  # first arg is the headline, remaining args are already-indented detail lines.
  echo "Error: $1" >&2
  shift
  for detail in "$@"; do
    echo "  $detail" >&2
  done
  exit 1
}

assert_eq() {
  local actual="$1" expected="$2" label="$3"
  if [[ "$actual" != "$expected" ]]; then
    fail "assertion failed: $label" "expected: $expected" "actual:   $actual"
  fi
  echo "PASS: $label"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Reads a single string-valued key out of a
# [package.metadata.binstall.overrides.<target>] table in Cargo.toml, without
# a jq/toml dependency (release.yml itself needs none for this shape either).
extract_binstall_field() {
  local target="$1" field="$2"
  awk -v section="[package.metadata.binstall.overrides.${target}]" -v field="$field" '
    $0 == section { in_section = 1; next }
    in_section && /^\[/ { in_section = 0 }
    in_section && $0 ~ "^" field "[[:space:]]*=" {
      line = $0
      sub("^" field "[[:space:]]*=[[:space:]]*\"", "", line)
      sub("\"[[:space:]]*$", "", line)
      print line
      exit
    }
  ' Cargo.toml
}

# ---------------------------------------------------------------------------
# [1] Version + asset_version derivation (release.yml "Extract version" step)
# ---------------------------------------------------------------------------
echo ""
echo "[1] Version extraction"
version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
if [[ -z "$version" ]]; then
  fail "no version found in Cargo.toml"
fi
asset_version="${version//+/-}"
echo "  version:       $version"
echo "  asset_version: $asset_version"

# P7: asset_version must be independently reproducible via `tr '+' '-'`, and
# sanitization must not be a no-op (the version must actually carry a '+').
independent_asset_version=$(printf '%s' "$version" | tr '+' '-')
assert_eq "$asset_version" "$independent_asset_version" "P7 asset_version == tr '+' '-' \$version"
if [[ "$asset_version" == "$version" ]]; then
  fail "P7: package.version has no '+' to sanitize; asset_version equals the raw version"
fi

# ---------------------------------------------------------------------------
# [2] Host target -> release-matrix name (release.yml `build` job matrix)
# ---------------------------------------------------------------------------
echo ""
echo "[2] Host target -> release-matrix name"
target=$(rustc -vV | awk '/^host: /{print $2}')
case "$target" in
  x86_64-unknown-linux-gnu) name="linux-x64" ;;
  x86_64-apple-darwin) name="macos-x64" ;;
  aarch64-apple-darwin) name="macos-arm64" ;;
  x86_64-pc-windows-msvc) name="windows-x64" ;;
  *)
    fail "unsupported host target for the release matrix" \
      "host target: $target" \
      "release.yml only builds: x86_64-unknown-linux-gnu, x86_64-apple-darwin, aarch64-apple-darwin, x86_64-pc-windows-msvc"
    ;;
esac
echo "  host target: $target -> matrix name: $name"

# ---------------------------------------------------------------------------
# [3] Build (release.yml "Build binary" step) -- full variant ONLY, no slim build.
# ---------------------------------------------------------------------------
echo ""
echo "[3] Build (--all-features only)"
cargo build --release --locked --target "$target" --all-features
echo "PASS: build"

# ---------------------------------------------------------------------------
# [4] Package (release.yml "Package (Unix)" / "Package (Windows)" steps),
#     including their sha256sum/shasum per-file checksum fallback.
# ---------------------------------------------------------------------------
echo ""
echo "[4] Package"
case "$name" in
  windows-x64)
    bin="target/$target/release/codanna.exe"
    dst="$test_dir/codanna-${asset_version}-${name}"
    archive="$dst.zip"
    mkdir "$dst"
    cp "$bin" "$dst/"
    cp LICENSE "$dst/"
    (cd "$test_dir" && 7z a "$(basename "$archive")" "$(basename "$dst")" >/dev/null)
    ;;
  *)
    bin="target/$target/release/codanna"
    dst="$test_dir/codanna-${asset_version}-${name}"
    archive="$dst.tar.xz"
    mkdir "$dst"
    cp "$bin" "$dst/"
    cp LICENSE "$dst/"
    tar -cJf "$archive" -C "$test_dir" "$(basename "$dst")"
    ;;
esac
echo "  archive: $archive"

sha256_of "$archive" > "$archive.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  sha512sum "$archive" | cut -d' ' -f1 > "$archive.sha512"
else
  shasum -a 512 "$archive" | cut -d' ' -f1 > "$archive.sha512"
fi
echo "PASS: package + per-file checksums"

# ---------------------------------------------------------------------------
# P2: archive filename == Cargo.toml pkg-url filename (kills "two sanitizers
# that disagree" -- the release.yml asset_version and the Cargo.toml literal).
# ---------------------------------------------------------------------------
echo ""
echo "[5] Assertion P2: archive filename matches binstall pkg-url"
pkg_url=$(extract_binstall_field "$target" "pkg-url")
if [[ -z "$pkg_url" ]]; then
  fail "no [package.metadata.binstall.overrides.$target] pkg-url found in Cargo.toml"
fi
expected_filename=$(basename "$(printf '%s' "$pkg_url" | sed 's/{ name }/codanna/g')")
actual_filename=$(basename "$archive")
assert_eq "$actual_filename" "$expected_filename" "P2 archive filename == binstall pkg-url filename"

# ---------------------------------------------------------------------------
# P6: archive's top-level directory == Cargo.toml bin-dir directory (kills
# "pkg-url fixed, bin-dir forgotten" -- binstall would download then fail
# extraction because it looks inside the wrong directory name).
# ---------------------------------------------------------------------------
echo ""
echo "[6] Assertion P6: archive top-level directory matches binstall bin-dir"
bin_dir=$(extract_binstall_field "$target" "bin-dir")
if [[ -z "$bin_dir" ]]; then
  fail "no [package.metadata.binstall.overrides.$target] bin-dir found in Cargo.toml"
fi
expected_top_dir=$(printf '%s' "$bin_dir" | sed 's/{ name }/codanna/g')
expected_top_dir="${expected_top_dir%%/*}"
# Capture the full listing, then take the first line via parameter expansion.
# Piping into `head -1` under `set -o pipefail` lets SIGPIPE (141) from the
# producer fail the assignment and abort the harness, making this check
# nondeterministic rather than deterministically green.
case "$name" in
  windows-x64) archive_listing=$(unzip -Z1 "$archive") ;;
  *) archive_listing=$(tar -tf "$archive") ;;
esac
actual_top_dir="${archive_listing%%$'\n'*}"
actual_top_dir="${actual_top_dir%/}"
actual_top_dir="${actual_top_dir%%/*}"
assert_eq "$actual_top_dir" "$expected_top_dir" "P6 archive top-level dir == binstall bin-dir directory"

# ---------------------------------------------------------------------------
# [7] Bulk checksums over the narrowed glob (release.yml "Generate bulk
#     checksums" step)
# ---------------------------------------------------------------------------
echo ""
echo "[7] Bulk checksums (narrowed glob: codanna-*.tar.xz codanna-*.zip)"
(
  cd "$test_dir"
  shopt -s nullglob
  archives=(codanna-*.tar.xz codanna-*.zip)
  shopt -u nullglob
  if [[ ${#archives[@]} -eq 0 ]]; then
    echo "Error: no packaged archives found matching codanna-*.tar.xz / codanna-*.zip" >&2
    exit 1
  fi
  # sha256sum-first, matching the "Generate bulk checksums" step this mirrors
  # (and the packaging steps, and scripts/install.sh) -- the fallbacks here and
  # in the workflow must not probe in opposite orders, or this harness would
  # pass on a host where the real workflow hard-fails.
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${archives[@]}" | tee SHA256SUMS
    sha512sum "${archives[@]}" | tee SHA512SUMS
  else
    shasum -a 256 "${archives[@]}" | tee SHA256SUMS
    shasum -a 512 "${archives[@]}" | tee SHA512SUMS
  fi
)
echo "PASS: bulk checksums"

echo ""
echo "[8] Assertion: checksum glob is narrowed (no sidecar lines, one line per archive)"
shopt -s nullglob
archives=("$test_dir"/codanna-*.tar.xz "$test_dir"/codanna-*.zip)
shopt -u nullglob
archive_count=${#archives[@]}
sha_line_count=$(wc -l < "$test_dir/SHA256SUMS" | tr -d ' ')
assert_eq "$sha_line_count" "$archive_count" "SHA256SUMS line count == archive count"
if grep -Eq '\.(sha256|sha512)$' "$test_dir/SHA256SUMS"; then
  fail "SHA256SUMS contains a sidecar checksum-file line (the glob was not narrowed)" \
    "$(grep -E '\.(sha256|sha512)$' "$test_dir/SHA256SUMS")"
fi
echo "PASS: SHA256SUMS has no *.sha256/*.sha512 sidecar lines"

# ---------------------------------------------------------------------------
# [9] Manifest generation (release.yml "Create manifest" step), including the
#     prefix-strip platform derivation this file exists to guard.
# ---------------------------------------------------------------------------
echo ""
echo "[9] Manifest generation (prefix-strip platform derivation)"
manifest="$test_dir/dist-manifest.json"
repo="rcrsr/rcrsr-codanna"
{
  echo "{"
  echo "  \"version\": \"$version\","
  echo "  \"artifacts\": ["

  first=true
  for file in "$test_dir"/codanna-*.tar.xz "$test_dir"/codanna-*.zip; do
    [[ -f "$file" ]] || continue
    base=$(basename "$file")

    # Exact prefix/suffix strip against the known sanitized asset_version --
    # see release.yml's own comment on why this must not be a '-'-separator
    # regex once the sanitized version itself contains a dash.
    platform="${base#codanna-${asset_version}-}"
    platform="${platform%.tar.xz}"
    platform="${platform%.zip}"
    if [[ "$platform" == "$base" ]]; then
      fail "filename does not carry the expected asset version" "file: $base" "asset_version: $asset_version"
    fi

    sha256=$(sha256_of "$file")
    url="https://github.com/${repo}/releases/download/v${version}/${base}"

    [[ "$first" == "false" ]] && echo ","
    first=false

    echo "    {"
    echo "      \"name\": \"$base\","
    echo "      \"url\": \"$url\","
    echo "      \"sha256\": \"$sha256\","
    echo "      \"platform\": \"$platform\""
    echo "    }"
  done

  echo "  ]"
  echo "}"
} > "$manifest"
cat "$manifest"
echo "PASS: manifest generated"

# ---------------------------------------------------------------------------
# P5: the manifest platform for the produced file must equal the host
# platform name exactly (kills the sed-based platform extraction the "Create
# manifest" step's prefix strip replaced).
# ---------------------------------------------------------------------------
echo ""
echo "[10] Assertion P5: manifest platform == host platform ($name)"
manifest_platform=$(awk -v want="\"$(basename "$archive")\"" '
  $0 ~ /"name":/ && index($0, want) { in_obj = 1 }
  in_obj && /"platform":/ {
    line = $0
    sub(/.*"platform": *"/, "", line)
    sub(/".*/, "", line)
    print line
    exit
  }
' "$manifest")
assert_eq "$manifest_platform" "$name" "P5 manifest platform for produced file"

echo ""
echo "[11] Assertion P7 (continued): archive name is not the bare unsanitized version"
bare_name_prefix="codanna-${version}-${name}"
if [[ "$(basename "$archive")" == "$bare_name_prefix".* ]]; then
  fail "archive still uses the bare, unsanitized version" \
    "archive: $(basename "$archive")" \
    "forbidden (bare) prefix: $bare_name_prefix"
fi
echo "PASS: archive name uses the sanitized asset_version, not the bare version"

# ---------------------------------------------------------------------------
# P3 (end-to-end, offline, zero network, no \$HOME pollution): a manifest
# whose url is file://, installed via scripts/install.sh, must produce a
# binary that reports the RAW '+'-bearing Cargo.toml version -- proving the
# delivered binary really originated from the sanitized-named archive.
# ---------------------------------------------------------------------------
echo ""
echo "[12] Assertion P3: offline end-to-end install via scripts/install.sh"
install_manifest="$test_dir/offline-dist-manifest.json"
archive_sha256=$(sha256_of "$archive")
cat > "$install_manifest" <<JSON
{
  "version": "$version",
  "artifacts": [
    {
      "name": "$(basename "$archive")",
      "url": "file://$archive",
      "sha256": "$archive_sha256",
      "platform": "$name"
    }
  ]
}
JSON

install_dir="$test_dir/bin"
if ! install_output=$(CODANNA_VERSION="v$version" \
     CODANNA_MANIFEST_URL="file://$install_manifest" \
     CODANNA_INSTALL_DIR="$install_dir" \
     sh scripts/install.sh 2>&1); then
  fail "scripts/install.sh failed against an offline file:// manifest" "$install_output"
fi
echo "$install_output"

if [[ ! -x "$install_dir/codanna" ]]; then
  fail "scripts/install.sh did not install an executable to $install_dir/codanna"
fi

installed_version_line=$("$install_dir/codanna" --version)
assert_eq "$installed_version_line" "codanna $version" "P3 installed binary reports the raw (+-bearing) version"

# ---------------------------------------------------------------------------
# Rosetta detection logic (extracted verbatim from scripts/install.sh's
# detect_platform(), not reimplemented): a stubbed sysctl reporting
# proc_translated=1 under a forced Darwin/x86_64 uname must resolve to
# macos-arm64; the real, unstubbed function on this host must stay
# idempotent (no stray override).
# ---------------------------------------------------------------------------
echo ""
echo "[13] Rosetta detection logic (install.sh detect_platform, extracted verbatim)"
detect_platform_src=$(awk '/^detect_platform\(\) \{/{p=1} p{print} p && /^}/{exit}' scripts/install.sh)
if [[ -z "$detect_platform_src" ]]; then
  fail "could not extract detect_platform() from scripts/install.sh"
fi
err() { echo "codanna: ERROR: $1" >&2; exit 1; }
eval "$detect_platform_src"

stub_bin="$test_dir/rosetta-stub-bin"
mkdir -p "$stub_bin"
cat > "$stub_bin/uname" <<'STUB'
#!/bin/sh
case "$1" in
  -s) echo "Darwin" ;;
  -m) echo "x86_64" ;;
  *) echo "unknown" ;;
esac
STUB
chmod +x "$stub_bin/uname"

cat > "$stub_bin/sysctl" <<'STUB'
#!/bin/sh
echo "1"
STUB
chmod +x "$stub_bin/sysctl"

stubbed_platform=$(PATH="$stub_bin:$PATH" detect_platform)
assert_eq "$stubbed_platform" "macos-arm64" "stubbed sysctl (proc_translated=1, forced Darwin/x86_64) resolves to macos-arm64"

# Assert against the INDEPENDENTLY derived host platform from step [2], not
# against another call to detect_platform: comparing the function to itself is
# trivially true and would pass for a detect_platform hardcoded to any value.
host_platform=$(detect_platform)
assert_eq "$host_platform" "$name" "unstubbed detect_platform matches the host matrix name from step [2] (no stray Rosetta override)"

# ---------------------------------------------------------------------------
# changelog-section.sh checks (BASIC.2.1: reuse the shared script, do not
# reimplement extraction inline).
# ---------------------------------------------------------------------------
echo ""
echo "[14] changelog-section.sh extraction"
if ! notes_0_12_0=$(contributing/scripts/changelog-section.sh "0.12.0"); then
  fail "changelog-section.sh failed to extract the existing '## [0.12.0]' section"
fi
if [[ -z "$notes_0_12_0" ]]; then
  fail "changelog-section.sh returned an empty body for '0.12.0'"
fi
echo "PASS: changelog-section.sh extracts the '## [0.12.0]' section body"

if contributing/scripts/changelog-section.sh "9.9.9-does-not-exist" >/dev/null 2>&1; then
  fail "changelog-section.sh should have failed for a version with no CHANGELOG section"
fi
echo "PASS: changelog-section.sh fails loudly for a missing version section"

echo ""
echo "[15] Release notes extraction for the live Cargo.toml version (release.yml \"Extract release notes from CHANGELOG.md\" step)"
if release_notes=$(contributing/scripts/changelog-section.sh "$version" 2>&1); then
  printf '%s\n' "$release_notes" > "$test_dir/RELEASE_NOTES.md"
  echo "PASS: extracted release notes for $version"
else
  echo "SKIP: CHANGELOG.md has no '## [$version]' section yet -- expected while $version is unreleased"
  echo "  (a real tag push fails on this in release.yml's \"Validate CHANGELOG section\""
  echo "   step, exactly as it should: rename '## [Unreleased]' to '## [$version] - <date>'"
  echo "   -- '+rcrsr.N' suffix included -- before tagging. A workflow_dispatch dry run"
  echo "   substitutes a placeholder instead of failing.)"
fi

# ---------------------------------------------------------------------------
# BASIC.2: do not reimplement the Cargo.toml binstall-template assertions in
# shell -- delegate to the Rust test that owns them.
# ---------------------------------------------------------------------------
echo ""
echo "[16] Cargo.toml binstall-template drift guard (delegated, not reimplemented)"
if command -v cargo >/dev/null 2>&1; then
  cargo test --test binstall_metadata_tests
  echo "PASS: binstall_metadata_tests"
else
  echo "SKIP: cargo not found on PATH"
fi

# ---------------------------------------------------------------------------
# Shadow-run guard. Every check above derives its values from THIS script's
# mirror of the packaging/manifest logic, so a regression introduced directly
# in release.yml would leave them all green. These assertions read the shipping
# workflow itself, so drift between the two files fails here rather than at
# tag time. Extend this step whenever a step above starts mirroring new logic.
# ---------------------------------------------------------------------------
echo ""
echo "[17] release.yml drift guard (assertions read the shipping workflow, not this mirror)"
workflow=".github/workflows/release.yml"

if ! grep -qF 'platform="${file#codanna-${asset_version}-}"' "$workflow"; then
  fail "release.yml no longer contains the exact prefix-strip platform derivation" \
       "expected literal: platform=\"\${file#codanna-\${asset_version}-}\"" \
       "in: $workflow" \
       "This harness mirrors that derivation; if the workflow changed, step [9] is now testing a stale copy."
fi
echo "PASS: release.yml still derives platform by exact prefix strip"

if grep -qF 's/codanna-[^-]*-' "$workflow"; then
  fail "release.yml has regressed to regex platform extraction" \
       "found the old sed pattern: s/codanna-[^-]*-\\([^.]*\\)\\..*/\\1/p" \
       "in: $workflow" \
       "That pattern splits on the first dash and breaks once the sanitized version contains one."
fi
echo "PASS: release.yml carries no regex-based filename parsing"

if ! grep -qF 'asset_version="${version//+/-}"' "$workflow"; then
  fail "release.yml no longer computes asset_version from version" \
       "expected literal: asset_version=\"\${version//+/-}\"" \
       "in: $workflow"
fi
echo "PASS: release.yml still computes asset_version as version with '+' replaced by '-'"

# Covers both branches of the step's sha256sum/shasum fallback -- a bare glob
# in either one is the same regression.
if grep -nE '(sha(256|512)sum|shasum -a (256|512)) codanna-\*( |$)' "$workflow" >/dev/null; then
  fail "release.yml bulk checksum glob has regressed to bare codanna-*" \
       "in: $workflow" \
       "A bare glob also matches the per-file .sha256/.sha512 sidecars, checksumming checksum files."
fi
echo "PASS: release.yml bulk checksum globs stay narrowed to archives"

# The harness's step [7] fallback is only a faithful mirror if the bulk
# checksum step has one too; a bare `shasum` there would hard-fail on a runner
# that ships sha256sum but not shasum, while this harness would pass on the
# same host. The literal asserted here is the sha256sum branch's own command
# line, which appears ONLY in that step -- matching on the enclosing
# `if command -v sha256sum` line instead would be vacuous, since the packaging
# steps and the manifest step's helper carry that same line.
if ! grep -qF 'sha256sum codanna-*.tar.xz codanna-*.zip | tee SHA256SUMS' "$workflow"; then
  fail "release.yml's bulk checksum step no longer prefers sha256sum over shasum" \
       "expected literal: sha256sum codanna-*.tar.xz codanna-*.zip | tee SHA256SUMS" \
       "in: $workflow" \
       "Step [7] mirrors that fallback; without it the harness passes where the workflow fails."
fi
echo "PASS: release.yml bulk checksum step keeps the sha256sum-first fallback"

echo ""
echo "================================="
echo "All release-workflow checks passed."
echo "================================="
