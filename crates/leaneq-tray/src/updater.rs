use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const RELEASE_URL: &str = "https://api.github.com/repos/hartelex/leaneq/releases/latest";
const DOWNLOAD_BASE: &str = "https://github.com/hartelex/leaneq/releases/download/";
const MAX_DOWNLOAD_SIZE: u64 = 100 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct StagedUpdate {
    pub version: String,
    pub executable: PathBuf,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: u64,
}

pub(super) fn check_for_update(
    current_version: &str,
    executable_path: &Path,
) -> Result<Option<StagedUpdate>, String> {
    let client = Client::builder()
        .user_agent("leaneq")
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("could not create update client: {error}"))?;

    let release = client
        .get(RELEASE_URL)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(|response| response.json::<Release>())
        .map_err(|error| format!("could not check GitHub Releases: {error}"))?;

    let release_version = parse_version(&release.tag_name)?;
    let current_version = Version::parse(current_version)
        .map_err(|error| format!("invalid current version: {error}"))?;
    if release_version <= current_version {
        return Ok(None);
    }

    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == "leaneq.exe")
        .ok_or_else(|| format!("release {} has no leaneq.exe asset", release.tag_name))?;
    let expected_url = format!("{DOWNLOAD_BASE}{}/leaneq.exe", release.tag_name);
    if asset.browser_download_url != expected_url
        || asset.size == 0
        || asset.size > MAX_DOWNLOAD_SIZE
    {
        return Err("the release executable has an invalid download URL or size".into());
    }

    let expected_digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "the release executable has no valid SHA-256 digest".to_string())?;

    let mut response = client
        .get(&asset.browser_download_url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| format!("could not download update: {error}"))?;
    if response
        .content_length()
        .is_some_and(|length| length != asset.size)
    {
        return Err("the downloaded update size does not match the release metadata".into());
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let executable_directory = executable_path
        .parent()
        .ok_or_else(|| "could not determine the executable directory".to_string())?;
    let staged_path =
        executable_directory.join(format!("leaneq-update-{}-{nonce}.exe", std::process::id()));
    let mut staged_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged_path)
        .map_err(|error| format!("could not create staged update: {error}"))?;

    let download_result =
        copy_and_hash(&mut response, &mut staged_file, asset.size).and_then(|actual_digest| {
            if actual_digest.eq_ignore_ascii_case(expected_digest) {
                staged_file
                    .flush()
                    .and_then(|()| staged_file.sync_all())
                    .map_err(|error| format!("could not save staged update: {error}"))
            } else {
                Err("the downloaded update failed SHA-256 verification".into())
            }
        });
    if let Err(error) = download_result {
        drop(staged_file);
        let _ = fs::remove_file(&staged_path);
        return Err(error);
    }

    Ok(Some(StagedUpdate {
        version: release_version.to_string(),
        executable: staged_path,
    }))
}

fn copy_and_hash(
    response: &mut impl Read,
    output: &mut impl Write,
    expected_size: u64,
) -> Result<String, String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 16 * 1024];
    let mut total_size = 0u64;
    loop {
        let count = response
            .read(&mut buffer)
            .map_err(|error| format!("could not read downloaded update: {error}"))?;
        if count == 0 {
            break;
        }
        total_size += count as u64;
        if total_size > expected_size || total_size > MAX_DOWNLOAD_SIZE {
            return Err("the downloaded update exceeds the expected size".into());
        }
        hasher.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|error| format!("could not write staged update: {error}"))?;
    }
    if total_size != expected_size {
        return Err("the downloaded update size does not match the release metadata".into());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn parse_version(tag: &str) -> Result<Version, String> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .map_err(|error| format!("invalid release version {tag:?}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tags_accept_v_prefix() {
        assert_eq!(parse_version("v1.2.3").unwrap(), Version::new(1, 2, 3));
    }

    #[test]
    fn hashes_only_exact_expected_size() {
        let mut input = &b"valid exe"[..];
        let mut output = Vec::new();
        let digest = copy_and_hash(&mut input, &mut output, 9).unwrap();
        assert_eq!(digest, format!("{:x}", Sha256::digest(b"valid exe")));
        assert_eq!(output, b"valid exe");
    }

    #[test]
    fn rejects_downloads_with_unexpected_size() {
        let mut input = &b"truncated"[..];
        let mut output = Vec::new();
        assert!(copy_and_hash(&mut input, &mut output, 10).is_err());
    }
}
