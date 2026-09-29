use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use tokio::process::Command;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeerIdentity {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub started_at: i64,
    pub proc_start: String,
    pub version: String,
    pub peer_protocol: u32,
    #[serde(default)]
    pub peer_features: Vec<String>,
    pub kind: String,
    pub entrypoint: String,
    pub pid_domain: String,
    pub messaging_socket_path: PathBuf,
    pub name: String,
    pub name_source: String,
    pub name_since: i64,
    pub status: String,
    pub updated_at: i64,
    pub status_updated_at: i64,
}

impl PeerIdentity {
    pub fn address(&self) -> String {
        format!("uds:{}", self.messaging_socket_path.display())
    }

    pub fn reference(&self) -> String {
        let hash = Sha256::digest(format!("session:{}", self.messaging_socket_path.display()));
        format!("{hash:x}")[..6].to_owned()
    }

    pub(crate) async fn is_live(&self) -> bool {
        self.peer_protocol == 1
            && self.name.chars().count() <= 64
            && !self.name.chars().any(char::is_control)
            && uuid::Uuid::parse_str(&self.session_id).is_ok()
            && self.pid_domain == pid_domain()
            && process_start(self.pid)
                .await
                .is_ok_and(|start| start == self.proc_start)
            && socket_metadata(&self.messaging_socket_path).is_ok()
    }
}

pub(crate) fn uid() -> u32 {
    // getuid는 인자를 받지 않으며 메모리에 접근하지 않는다.
    unsafe { libc::getuid() }
}

pub(crate) fn pid_domain() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    }
}

pub(crate) async fn process_start(pid: u32) -> Result<String> {
    let output = Command::new("/bin/ps")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .kill_on_drop(true)
        .output()
        .await?;
    let start = String::from_utf8(output.stdout)?.trim().to_owned();
    if !output.status.success() || start.is_empty() {
        bail!("peer process is no longer running");
    }
    Ok(start)
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
        bail!("peer directory must be owner-only and must not be a symlink");
    }
    Ok(())
}

pub(crate) fn socket_metadata(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("peer socket path must be absolute");
    }
    let parent = path.parent().context("peer socket has no parent")?;
    let directory = fs::symlink_metadata(parent)?;
    let socket = fs::symlink_metadata(path)?;
    if !directory.is_dir()
        || directory.uid() != uid()
        || directory.mode() & 0o077 != 0
        || !socket.file_type().is_socket()
        || socket.uid() != uid()
        || socket.mode() & 0o077 != 0
    {
        bail!("peer socket ownership or permissions are invalid");
    }
    Ok(())
}

pub(crate) fn read_owned_file(path: &Path, max_bytes: u64, private: bool) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != uid()
        || metadata.len() > max_bytes
        || private && metadata.mode() & 0o077 != 0
        || metadata.mode() & 0o022 != 0
    {
        bail!("peer file ownership, size or permissions are invalid");
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    (&mut file).take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("peer file exceeds size limit");
    }
    Ok(bytes)
}

pub(crate) fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("peer file has no parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn key_path(root: &Path, peer: &PeerIdentity) -> PathBuf {
    let hash = Sha256::digest(peer.messaging_socket_path.to_string_lossy().as_bytes());
    root.join(format!("{}.{hash:x}.key", peer.pid))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerKey {
    pub peer_token: String,
    pub proc_start: String,
    pub pid_domain: String,
}

pub(crate) async fn list(root: &Path) -> Result<Vec<PeerIdentity>> {
    let mut peers = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(bytes) = read_owned_file(&path, 16_384, false) else {
            continue;
        };
        let Ok(peer) = serde_json::from_slice::<PeerIdentity>(&bytes) else {
            continue;
        };
        if peer.pid == pid && peer.is_live().await {
            peers.push(peer);
        }
        if peers.len() >= 1024 {
            break;
        }
    }
    peers.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    Ok(peers)
}

pub(crate) async fn resolve(root: &Path, target: &str) -> Result<PeerIdentity> {
    let matches = list(root)
        .await?
        .into_iter()
        .filter(|peer| {
            target == peer.name
                || target == peer.session_id
                || target == peer.reference()
                || target == format!("{} [{}]", peer.name, peer.reference())
                || target == peer.address()
                || target == peer.messaging_socket_path.to_string_lossy()
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [peer] => Ok(peer.clone()),
        [] => bail!("no live peer matches target; refresh the session list"),
        _ => bail!("peer name is ambiguous; use its full socket address"),
    }
}

pub(crate) struct Registration {
    pub root: PathBuf,
    pub identity: PeerIdentity,
    pub token: String,
    _socket: SocketGuard,
    key_inode: u64,
}

pub(crate) struct SocketGuard {
    pub path: PathBuf,
    pub inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.ino() == self.inode && metadata.uid() == uid())
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Registration {
    pub fn publish(
        root: PathBuf,
        identity: PeerIdentity,
        socket: SocketGuard,
        previous: Option<&PeerIdentity>,
    ) -> Result<Self> {
        private_directory(&root)?;
        let registry_path = root.join(format!("{}.json", identity.pid));
        if registry_path.exists() {
            let existing: PeerIdentity =
                serde_json::from_slice(&read_owned_file(&registry_path, 16_384, false)?)?;
            let stale = existing.pid == identity.pid
                && existing.pid_domain == identity.pid_domain
                && existing.proc_start != identity.proc_start;
            let replacing = previous.is_some_and(|previous| {
                previous.pid == existing.pid
                    && previous.proc_start == existing.proc_start
                    && previous.session_id == existing.session_id
                    && previous.messaging_socket_path == existing.messaging_socket_path
            });
            if !stale && !replacing {
                bail!("this process already has a registered Claude peer");
            }
        }
        let token = rand::random::<[u8; 16]>()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut registration = Self {
            root,
            identity,
            token,
            _socket: socket,
            key_inode: 0,
        };
        let key = PeerKey {
            peer_token: registration.token.clone(),
            proc_start: registration.identity.proc_start.clone(),
            pid_domain: registration.identity.pid_domain.clone(),
        };
        write_private_json(&key_path(&registration.root, &registration.identity), &key)?;
        registration.key_inode =
            fs::symlink_metadata(key_path(&registration.root, &registration.identity))?.ino();
        write_private_json(&registry_path, &registration.identity)?;
        Ok(registration)
    }

    pub fn update(&self, identity: &PeerIdentity) -> Result<()> {
        let path = self.root.join(format!("{}.json", identity.pid));
        if path.exists() {
            let current: PeerIdentity =
                serde_json::from_slice(&read_owned_file(&path, 16_384, false)?)?;
            anyhow::ensure!(
                current.session_id == self.identity.session_id
                    && current.proc_start == self.identity.proc_start
                    && current.messaging_socket_path == self.identity.messaging_socket_path,
                "peer registration belongs to a different generation"
            );
        }
        write_private_json(&path, identity)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let path = self.root.join(format!("{}.json", self.identity.pid));
        let owned = read_owned_file(&path, 16_384, false)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PeerIdentity>(&bytes).ok())
            .is_some_and(|peer| {
                peer.session_id == self.identity.session_id
                    && peer.proc_start == self.identity.proc_start
                    && peer.messaging_socket_path == self.identity.messaging_socket_path
            });
        if owned {
            let _ = fs::remove_file(path);
        }
        let key = key_path(&self.root, &self.identity);
        if fs::symlink_metadata(&key)
            .is_ok_and(|metadata| metadata.ino() == self.key_inode && metadata.uid() == uid())
        {
            let _ = fs::remove_file(key);
        }
    }
}
