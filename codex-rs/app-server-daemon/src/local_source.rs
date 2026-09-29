//! npm 설치본을 갱신 출처로 사용한다. 네트워크 설치기나 전역 npm 설치는 실행하지 않는다.

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Source {
    package_dir: PathBuf,
    version: semver::Version,
}

pub(crate) enum Selection {
    PreferNewest,
    Explicit,
}

fn descriptor(home: &Path) -> PathBuf {
    home.join(crate::STATE_DIR_NAME).join("source.json")
}

fn read_source(home: &Path) -> Result<Source> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(descriptor(home))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 16_384,
        "invalid local daemon source descriptor"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // getuid는 메모리에 접근하지 않는 인자 없는 시스템 호출이다.
        let uid = unsafe { libc::getuid() };
        anyhow::ensure!(
            metadata.uid() == uid && metadata.mode() & 0o077 == 0,
            "daemon source descriptor must be owner-only"
        );
    }
    let mut bytes = Vec::new();
    file.take(16_385).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 16_384,
        "daemon source descriptor is too large"
    );
    let source: Source =
        serde_json::from_slice(&bytes).context("invalid local daemon source descriptor")?;
    anyhow::ensure!(
        source.package_dir.is_absolute(),
        "daemon source must be absolute"
    );
    anyhow::ensure!(
        !source
            .package_dir
            .starts_with(crate::managed_install::package_root(home)),
        "managed daemon cannot be its own update source"
    );
    Ok(source)
}

pub(crate) fn entrypoint(root: &Path) -> Result<PathBuf> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("codex-package.json"))?)?;
    let relative = if cfg!(windows) {
        "bin/claudex.exe"
    } else {
        "bin/claudex"
    };
    anyhow::ensure!(
        manifest["layoutVersion"] == 1
            && manifest["variant"] == "claudex"
            && manifest["target"] == crate::prepare_install::platform_target()?
            && manifest["entrypoint"] == relative,
        "daemon source must be a matching Claudex native package"
    );
    let program = root.join(relative);
    anyhow::ensure!(
        program.is_file() && program.canonicalize()?.starts_with(root.canonicalize()?),
        "daemon entrypoint escapes its package or is missing"
    );
    Ok(program)
}

pub(crate) fn remember(home: &Path, package: &Path, selection: Selection) -> Result<()> {
    crate::prepare_install::validate_package(package)?;
    let package_dir = package.canonicalize()?;
    let managed = crate::managed_install::package_root(home);
    if let Ok(managed) = managed.canonicalize()
        && package_dir.starts_with(managed)
    {
        return Ok(());
    }
    let manifest: codex_install_context::CodexPackageManifest =
        serde_json::from_slice(&fs::read(package.join("codex-package.json"))?)?;
    let source = Source {
        package_dir,
        version: semver::Version::parse(&manifest.version.to_string())?,
    };
    if matches!(selection, Selection::PreferNewest)
        && let Ok(previous) = read_source(home)
        && previous.version > source.version
    {
        return Ok(());
    }
    let destination = descriptor(home);
    let parent = destination
        .parent()
        .context("daemon source has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, &source)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist(destination)?;
    Ok(())
}

pub(crate) fn has_source(home: &Path) -> bool {
    read_source(home).is_ok()
}

pub(crate) fn eligible(home: &Path, program: &Path) -> bool {
    let root = crate::managed_install::package_root(home);
    let Ok(release) = root.join("current").canonicalize() else {
        return false;
    };
    let Ok(releases) = root.join("releases").canonicalize() else {
        return false;
    };
    let Ok(entry) = entrypoint(&release).and_then(|path| Ok(path.canonicalize()?)) else {
        return false;
    };
    let Ok(program) = program.canonicalize() else {
        return false;
    };
    release.parent() == Some(releases.as_path()) && has_source(home) && entry == program
}

pub(crate) fn select(root: &Path, release: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let temp = tempfile::TempDir::new_in(root)?;
        let link = temp.path().join("current");
        std::os::unix::fs::symlink(release, &link)?;
        fs::rename(link, root.join("current"))?;
    }
    #[cfg(windows)]
    {
        crate::prepare_install::select_windows_release(root, release)?;
    }
    Ok(())
}

/// 설치 잠금 안에서 완전한 package를 준비하고 current를 원자적으로 바꾼다.
pub(crate) async fn sync(root: &Path, expected: &Path) -> Result<()> {
    let home = root
        .parent()
        .and_then(Path::parent)
        .context("daemon package root has no home")?;
    let source = read_source(home)?;
    crate::prepare_install::validate_package(&source.package_dir)?;
    let _lock = crate::install_lock::acquire_install_lock(root).await?;
    anyhow::ensure!(
        root.join("current").canonicalize()? == expected,
        "daemon selection changed; retry the update"
    );
    let releases = root.join("releases");
    fs::create_dir_all(&releases)?;
    let source_dir = source.package_dir.clone();
    let selected = expected.to_path_buf();
    let releases_for_copy = releases.clone();
    let prepared =
        tokio::task::spawn_blocking(move || -> Result<Option<(tempfile::TempDir, String)>> {
            let source_digest = crate::prepare_install::package_tree(&source_dir, None)?;
            if crate::prepare_install::package_tree(&selected, None)? == source_digest {
                return Ok(None);
            }
            let stage = tempfile::Builder::new()
                .prefix(".local-sync.")
                .tempdir_in(releases_for_copy)?;
            let copied = crate::prepare_install::package_tree(&source_dir, Some(stage.path()))?;
            crate::prepare_install::validate_package(stage.path())?;
            anyhow::ensure!(
                source_digest == copied
                    && crate::prepare_install::package_tree(&source_dir, None)? == copied,
                "npm package changed while copying; keeping current daemon"
            );
            Ok(Some((stage, copied)))
        })
        .await??;
    let Some((stage, digest)) = prepared else {
        return Ok(());
    };
    let manifest: codex_install_context::CodexPackageManifest =
        serde_json::from_slice(&fs::read(stage.path().join("codex-package.json"))?)?;
    let version = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        crate::managed_install::managed_codex_version(&entrypoint(stage.path())?),
    )
    .await
    .context("timed out checking the newly staged Claudex executable version")??;
    anyhow::ensure!(
        manifest.version.to_string() == version,
        "local Claudex package version differs from its executable"
    );
    anyhow::ensure!(
        read_source(home)? == source && root.join("current").canonicalize()? == expected,
        "daemon source or selection changed; retry"
    );
    let target = crate::prepare_install::platform_target()?;
    let release = releases.join(format!("local-{digest}-{target}"));
    if release.exists() {
        anyhow::ensure!(
            !release.symlink_metadata()?.file_type().is_symlink(),
            "daemon release must not be a symlink"
        );
        anyhow::ensure!(
            crate::prepare_install::package_tree(&release, None)? == digest,
            "existing daemon release contents differ"
        );
    } else {
        fs::rename(stage.path(), &release)?;
    }
    select(root, &release)
}

#[cfg(test)]
#[path = "local_source_tests.rs"]
mod tests;
