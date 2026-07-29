//! Guards the `[package.metadata.binstall]` templates in Cargo.toml against
//! drift from the release workflow's asset-name sanitization.
//!
//! cargo-binstall's `{ ... }` templating supports only plain substitution --
//! there is no transform/filter syntax, so it cannot derive a sanitized
//! (`+` -> `-`) version from the raw `{ version }` key on its own. The
//! sanitized version must therefore appear as a hardcoded literal in
//! `pkg-url` (filename segment) and `bin-dir`. These tests assert that the
//! literal in Cargo.toml actually matches `version.replace('+', "-")`, and
//! that raw `{ version }` is confined to the `download/v{ version }/` URL
//! path segment (the git tag), never leaking into a filename or bin-dir.

use std::fs;
use std::path::Path;

/// Rust target triple paired with the release-matrix platform name its assets
/// must carry (`.github/workflows/release.yml`, `build` job matrix).
///
/// The platform half is asserted, not just carried: without it all four
/// overrides could name `-linux-x64` archives and every other assertion in
/// this file would still pass, leaving macOS and Windows users resolving a
/// Linux binary.
const OVERRIDE_TARGETS: [(&str, &str); 4] = [
    ("x86_64-unknown-linux-gnu", "linux-x64"),
    ("x86_64-apple-darwin", "macos-x64"),
    ("aarch64-apple-darwin", "macos-arm64"),
    ("x86_64-pc-windows-msvc", "windows-x64"),
];

fn load_cargo_toml() -> toml::Value {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let contents =
        fs::read_to_string(&manifest_path).expect("Cargo.toml must be readable at test time");
    // `str::parse::<toml::Value>()` (via `FromStr`) deserializes a single TOML
    // *value* expression, not a whole document -- `toml::from_str` is the
    // document-level entry point and is what a `[package]`-rooted file needs.
    toml::from_str::<toml::Value>(&contents).expect("Cargo.toml must be valid TOML")
}

fn package_version(manifest: &toml::Value) -> String {
    manifest["package"]["version"]
        .as_str()
        .expect("package.version must be a string")
        .to_string()
}

/// Asserts that `{ version }` (raw, untransformed) appears in `pkg_url`
/// exactly once, and only inside the `download/v{ version }/` URL path
/// segment. This is the falsifiable guard: it must reject a template that
/// places `{ version }` in the filename position, not merely accept the
/// current well-formed templates.
fn assert_version_only_in_tag_segment(pkg_url: &str) {
    let occurrences = pkg_url.matches("{ version }").count();
    assert_eq!(
        occurrences, 1,
        "pkg-url must reference raw `{{ version }}` exactly once (in the tag segment), found {occurrences} in: {pkg_url}"
    );

    let tag_segment = "download/v{ version }/";
    assert!(
        pkg_url.contains(tag_segment),
        "pkg-url must carry raw `{{ version }}` inside a `download/v{{ version }}/` tag segment: {pkg_url}"
    );

    // Belt-and-braces restatement, not the load-bearing check: given exactly
    // one occurrence and a present tag segment, that occurrence is already
    // provably the tag-segment one, so this can never fire on its own. The
    // assertions above are what actually reject a leaked raw `{ version }`.
    let without_tag_segment = pkg_url.replacen(tag_segment, "", 1);
    assert!(
        !without_tag_segment.contains("{ version }"),
        "pkg-url must not reference raw `{{ version }}` outside the `download/v{{ version }}/` tag segment: {pkg_url}"
    );
}

#[test]
fn overrides_pkg_url_and_bin_dir_use_sanitized_literal_version() {
    let manifest = load_cargo_toml();
    let version = package_version(&manifest);
    let asset_version = version.replace('+', "-");

    // Provenance assertion: the sanitized literal must actually differ from
    // the raw version and must still carry the fork's `rcrsr` identity, not
    // merely be "some literal that happens to look plausible".
    assert_ne!(
        asset_version, version,
        "package.version has no '+' to sanitize; this test's premise (raw vs. sanitized version) no longer holds"
    );
    assert!(
        asset_version.contains("rcrsr"),
        "sanitized asset_version must retain fork identity ('rcrsr'), got: {asset_version}"
    );

    let overrides = manifest["package"]["metadata"]["binstall"]["overrides"]
        .as_table()
        .expect("package.metadata.binstall.overrides must be a table");

    for (target, platform) in OVERRIDE_TARGETS {
        let entry = overrides
            .get(target)
            .unwrap_or_else(|| panic!("missing binstall override for target: {target}"));

        let pkg_url = entry["pkg-url"]
            .as_str()
            .unwrap_or_else(|| panic!("{target}: pkg-url must be a string"));
        let bin_dir = entry["bin-dir"]
            .as_str()
            .unwrap_or_else(|| panic!("{target}: bin-dir must be a string"));

        assert!(
            pkg_url.contains(&asset_version),
            "{target}: pkg-url must contain the sanitized literal version {asset_version:?}, got: {pkg_url}"
        );
        assert!(
            bin_dir.contains(&asset_version),
            "{target}: bin-dir must contain the sanitized literal version {asset_version:?}, got: {bin_dir}"
        );

        // Each override must point at ITS OWN platform's assets. Anchored on
        // the sanitized version so a stray `-linux-x64` elsewhere in the URL
        // cannot satisfy it.
        let expected_stem = format!("{asset_version}-{platform}");
        assert!(
            pkg_url.contains(&expected_stem),
            "{target}: pkg-url must name the {platform} archive ({expected_stem:?}), got: {pkg_url}"
        );
        assert!(
            bin_dir.contains(&expected_stem),
            "{target}: bin-dir must name the {platform} directory ({expected_stem:?}), got: {bin_dir}"
        );

        assert_version_only_in_tag_segment(pkg_url);

        assert_eq!(
            bin_dir.matches("{ version }").count(),
            0,
            "{target}: bin-dir must not reference raw `{{ version }}` at all, got: {bin_dir}"
        );
    }
}

#[test]
fn top_level_bin_dir_is_removed() {
    let manifest = load_cargo_toml();
    let binstall = manifest["package"]["metadata"]["binstall"]
        .as_table()
        .expect("package.metadata.binstall must be a table");

    assert!(
        !binstall.contains_key("bin-dir"),
        "top-level [package.metadata.binstall] bin-dir is unreachable for all four \
         supported targets (each override supplies its own) and is a fifth site \
         deriving a name from the raw version; it must be deleted"
    );
    assert!(
        binstall.contains_key("pkg-fmt"),
        "top-level pkg-fmt must remain (only bin-dir was deleted)"
    );
}

/// Negative control: proves `assert_version_only_in_tag_segment` is
/// falsifiable, not vacuously true. A template that leaks raw `{ version }`
/// into the filename position must be rejected.
#[test]
#[should_panic(expected = "must reference raw `{ version }` exactly once")]
fn rejects_raw_version_leaking_into_filename_position() {
    let bad_pkg_url = "https://github.com/rcrsr/rcrsr-codanna/releases/download/v{ version }/codanna-{ version }-linux-x64.tar.xz";
    assert_version_only_in_tag_segment(bad_pkg_url);
}

/// Second negative control: a template with exactly one raw `{ version }`
/// reference, but placed in the filename rather than the tag segment (e.g.
/// a hardcoded raw tag alongside a still-templated filename), must also be
/// rejected -- proving the tag-segment confinement check, not just the
/// occurrence count, is load-bearing.
#[test]
#[should_panic(expected = "must carry raw")]
fn rejects_raw_version_outside_tag_segment() {
    let bad_pkg_url = "https://github.com/rcrsr/rcrsr-codanna/releases/download/v0.12.0-rcrsr.1/codanna-{ version }-linux-x64.tar.xz";
    assert_version_only_in_tag_segment(bad_pkg_url);
}
