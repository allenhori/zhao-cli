//! `zhao update`: replaces the current binary with a release archive
//! fetched from GitHub Releases (see issue #28) -- the one command that
//! makes a network call at all. Every other zhao command stays fully
//! offline, per the README's "What it doesn't do" section, which this
//! command is the deliberate, explicit exception to: it only runs when
//! a user or the Cloud Agent (per ADR 0009) explicitly invokes it,
//! never as a side effect of `check`/`diff`/`lineage`.
//!
//! Kept deliberately simple: updates by release tag, not a semver-range
//! resolver. No arguments installs the latest stable release; `--nightly`
//! installs the current moving `nightly` tag; a version/tag argument pins
//! to that exact release.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cli::UpdateArgs;

/// `owner/repo`, for both the GitHub Releases URLs this command
/// downloads from.
const REPO: &str = "allenhori/zhao-cli";

/// Exit code for "the binary was actually replaced."
const EXIT_OK: u8 = 0;

/// Runs `zhao update` and returns the process exit code.
pub fn run(args: &UpdateArgs) -> ExitCode {
    let tag = if args.nightly {
        "nightly".to_string()
    } else if let Some(version) = &args.version {
        version.clone()
    } else {
        "latest".to_string()
    };

    match update(&tag) {
        Ok(installed_path) => {
            println!(
                "Updated {} to {tag} -- run `zhao --version` to confirm.",
                installed_path.display()
            );
            ExitCode::from(EXIT_OK)
        }
        Err(message) => crate::engine::fail(&message),
    }
}

/// Downloads `tag`'s release archive for the current platform, extracts
/// the `zhao` binary from it, and atomically replaces the currently
/// running executable with it. Never leaves a broken/partial binary in
/// place: every failure before the final rename leaves the existing
/// binary completely untouched, and the rename itself is the one
/// operation that actually swaps it in.
fn update(tag: &str) -> Result<PathBuf, String> {
    let target = platform_target()?;
    let archive_name = archive_name(&target);
    let url = download_url(tag, &archive_name);

    let archive_bytes = download(&url).map_err(|err| {
        format!(
            "could not download {url}: {err} -- check that {tag:?} is a real release tag at \
             https://github.com/{REPO}/releases"
        )
    })?;

    let binary_bytes = extract_binary(&archive_bytes, &target)?;

    let current_exe = std::env::current_exe()
        .map_err(|err| format!("could not determine the current executable's path: {err}"))?;
    replace_binary(&current_exe, &binary_bytes)?;

    Ok(current_exe)
}

/// The four target triples zhao actually publishes release binaries
/// for -- matches `scripts/install.sh`'s own platform detection and
/// `.github/workflows/release.yml`'s build matrix exactly, so this
/// can't silently drift out of sync with what a release actually
/// contains.
fn platform_target() -> Result<String, String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin".to_string()),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin".to_string()),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu".to_string()),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc".to_string()),
        _ => Err(format!(
            "no released binary for {os}/{arch} -- build from source instead: \
             cargo install --git https://github.com/{REPO}"
        )),
    }
}

/// The release archive's filename for `target` -- `.zip` on Windows
/// (matching `Compress-Archive`'s output in `release.yml`), `.tar.gz`
/// everywhere else.
fn archive_name(target: &str) -> String {
    if cfg!(windows) {
        format!("zhao-{target}.zip")
    } else {
        format!("zhao-{target}.tar.gz")
    }
}

/// `tag == "latest"` uses GitHub's `/releases/latest/download/` alias
/// (always the newest non-prerelease release, same as
/// `scripts/install.sh`'s own default); any other tag (including
/// `"nightly"`, a real tag like every other release) is a direct
/// `/releases/download/<tag>/` URL.
fn download_url(tag: &str, archive_name: &str) -> String {
    if tag == "latest" {
        format!("https://github.com/{REPO}/releases/latest/download/{archive_name}")
    } else {
        format!("https://github.com/{REPO}/releases/download/{tag}/{archive_name}")
    }
}

/// Downloads `url`'s full response body. A non-2xx status (e.g. a
/// nonexistent tag/asset, a 404) is already a clear `Err` from `ureq`
/// itself -- no extra status-code handling needed here.
fn download(url: &str) -> Result<Vec<u8>, String> {
    let response = ureq::get(url).call().map_err(|err| err.to_string())?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    Ok(bytes)
}

/// Extracts the `zhao`/`zhao.exe` binary's raw bytes out of a
/// downloaded release archive. `target` only matters for the entry
/// name's error message -- it's not re-validated against the archive's
/// own contents. Cfg-gated directly (rather than a runtime
/// `cfg!(windows)` branch over two always-compiled implementations),
/// matching `Cargo.toml`'s own per-platform `[target.'cfg(...)']`
/// dependency split: each build only ever needs its own platform's
/// extractor, so there's no reason to compile the other one in at all,
/// dead-code stub or otherwise.
#[cfg(not(windows))]
fn extract_binary(archive_bytes: &[u8], target: &str) -> Result<Vec<u8>, String> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| format!("could not read {target}'s release archive: {err}"))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|err| format!("could not read a release archive entry: {err}"))?;
        let path = entry
            .path()
            .map_err(|err| format!("could not read a release archive entry's path: {err}"))?;
        if path.file_name().and_then(|name| name.to_str()) == Some("zhao") {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|err| format!("could not read the zhao binary from the archive: {err}"))?;
            return Ok(bytes);
        }
    }
    Err(format!(
        "the {target} release archive doesn't contain a `zhao` binary"
    ))
}

/// The Windows counterpart of the `.tar.gz` extractor above -- same
/// contract (`target` only for its error message's context), same
/// cfg-gating rationale.
#[cfg(windows)]
fn extract_binary(archive_bytes: &[u8], target: &str) -> Result<Vec<u8>, String> {
    let reader = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|err| format!("could not read {target}'s release archive: {err}"))?;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|err| format!("could not read a release archive entry: {err}"))?;
        if file.name() == "zhao.exe" {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|err| format!("could not read the zhao binary from the archive: {err}"))?;
            return Ok(bytes);
        }
    }
    Err(format!(
        "the {target} release archive doesn't contain a zhao.exe binary"
    ))
}

/// Writes `new_binary_bytes` to a temp file in `current_exe`'s own
/// directory (guaranteeing the final rename is on the same filesystem,
/// so it's atomic), makes it executable on Unix, then renames it over
/// `current_exe`. Renaming over the file backing the currently
/// *running* process is safe on both Unix (the OS keeps the old inode
/// alive for the still-running process; the new file only takes effect
/// on the next launch) and Windows (renaming, unlike deleting or
/// truncating-in-place, doesn't require exclusive access to a mapped
/// executable). Every step before the rename can fail without touching
/// `current_exe` at all; the rename itself is the one moment the swap
/// actually happens, and it's a single filesystem operation, not a
/// multi-step window where a partial binary could be left in place.
fn replace_binary(current_exe: &Path, new_binary_bytes: &[u8]) -> Result<(), String> {
    let dir = current_exe.parent().ok_or_else(|| {
        format!(
            "could not determine the directory containing {}",
            current_exe.display()
        )
    })?;

    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|err| format!("could not create a temp file in {}: {err}", dir.display()))?;
    temp.write_all(new_binary_bytes)
        .map_err(|err| format!("could not write the downloaded binary to disk: {err}"))?;
    temp.flush()
        .map_err(|err| format!("could not write the downloaded binary to disk: {err}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = temp
            .as_file()
            .metadata()
            .map_err(|err| format!("could not read the downloaded binary's metadata: {err}"))?
            .permissions();
        perms.set_mode(0o755);
        temp.as_file()
            .set_permissions(perms)
            .map_err(|err| format!("could not make the downloaded binary executable: {err}"))?;
    }

    temp.persist(current_exe).map_err(|err| {
        format!(
            "could not replace {}: {err} -- the previous binary is still in place, untouched",
            current_exe.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_url_for_latest_uses_the_latest_download_alias() {
        assert_eq!(
            download_url("latest", "zhao-x86_64-apple-darwin.tar.gz"),
            "https://github.com/allenhori/zhao-cli/releases/latest/download/\
             zhao-x86_64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn download_url_for_a_specific_tag_uses_the_tagged_download_url() {
        assert_eq!(
            download_url("v0.1.1", "zhao-x86_64-apple-darwin.tar.gz"),
            "https://github.com/allenhori/zhao-cli/releases/download/v0.1.1/\
             zhao-x86_64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn download_url_for_nightly_is_just_the_nightly_tag() {
        assert_eq!(
            download_url("nightly", "zhao-x86_64-unknown-linux-gnu.tar.gz"),
            "https://github.com/allenhori/zhao-cli/releases/download/nightly/\
             zhao-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn archive_name_matches_the_release_pipelines_naming() {
        // This test's own platform's extension, not a hardcoded one --
        // `archive_name` itself is `cfg!(windows)`-branching, so
        // asserting against a literal `.tar.gz` would be wrong when
        // this test happens to run on a Windows CI runner.
        let expected_ext = if cfg!(windows) { "zip" } else { "tar.gz" };
        assert_eq!(
            archive_name("x86_64-unknown-linux-gnu"),
            format!("zhao-x86_64-unknown-linux-gnu.{expected_ext}")
        );
    }

    /// `platform_target` should recognize this test's own platform --
    /// exercised against `std::env::consts::OS`/`ARCH` directly, since
    /// those are exactly what it reads.
    #[test]
    fn platform_target_recognizes_a_supported_platform() {
        if matches!(
            (std::env::consts::OS, std::env::consts::ARCH),
            ("macos", "aarch64" | "x86_64") | ("linux", "x86_64") | ("windows", "x86_64")
        ) {
            assert!(platform_target().is_ok());
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn extract_tar_gz_finds_the_zhao_binary_by_name() {
        use std::io::Write;

        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let contents = b"pretend binary contents";
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "zhao", &contents[..])
                .expect("should append zhao entry");
            builder.finish().expect("should finish tar");
        }
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::default());
            encoder.write_all(&tar_bytes).expect("should gzip");
            encoder.finish().expect("should finish gzip");
        }

        let extracted =
            extract_binary(&gz_bytes, "x86_64-unknown-linux-gnu").expect("should extract");
        assert_eq!(extracted, b"pretend binary contents");
    }

    #[cfg(not(windows))]
    #[test]
    fn extract_tar_gz_produces_a_clear_error_when_no_zhao_entry_exists() {
        use std::io::Write;

        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let contents = b"unrelated";
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, "README.md", &contents[..])
                .expect("should append entry");
            builder.finish().expect("should finish tar");
        }
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::default());
            encoder.write_all(&tar_bytes).expect("should gzip");
            encoder.finish().expect("should finish gzip");
        }

        let result = extract_binary(&gz_bytes, "x86_64-unknown-linux-gnu");
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    /// Acceptance criterion: a failed replace never leaves a broken/
    /// partial binary in place -- here, a `current_exe` whose parent
    /// directory doesn't exist, so the temp-file step itself fails
    /// before anything ever touches the (nonexistent) target path.
    #[test]
    fn replace_binary_fails_cleanly_when_the_target_directory_does_not_exist() {
        let fake_exe = std::path::Path::new("/definitely/does/not/exist/zhao");
        let result = replace_binary(fake_exe, b"new binary");
        assert!(result.is_err());
    }

    #[test]
    fn replace_binary_actually_replaces_the_files_contents() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let exe_path = dir.path().join("zhao");
        std::fs::write(&exe_path, b"old binary").expect("should write initial binary");

        replace_binary(&exe_path, b"new binary").expect("should replace");

        let contents = std::fs::read(&exe_path).expect("should read replaced binary");
        assert_eq!(contents, b"new binary");
    }

    #[cfg(unix)]
    #[test]
    fn replace_binary_makes_the_new_file_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("should create temp dir");
        let exe_path = dir.path().join("zhao");
        std::fs::write(&exe_path, b"old binary").expect("should write initial binary");

        replace_binary(&exe_path, b"new binary").expect("should replace");

        let mode = std::fs::metadata(&exe_path)
            .expect("should stat replaced binary")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "expected the file to be executable");
    }
}
