use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const TRUST_SCHEMA_VERSION: u32 = 1;
const MAX_TAILSCALE_STATUS_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentity {
    key: String,
    label: String,
    tailscale: bool,
}

impl ClientIdentity {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn remember_detail(&self) -> String {
        if self.tailscale {
            format!("{} via Tailscale; no password is stored", self.label)
        } else {
            format!("Trust address {}; no password is stored", self.label)
        }
    }

    fn address(ip: IpAddr) -> Self {
        let ip = normalize_ip(ip);
        Self {
            key: format!("address:{ip}"),
            label: ip.to_string(),
            tailscale: false,
        }
    }

    #[cfg(test)]
    fn tailscale(stable_id: &str, label: &str) -> Self {
        Self {
            key: format!("tailscale:{stable_id}"),
            label: label.to_string(),
            tailscale: true,
        }
    }
}

#[derive(Clone, Default)]
pub struct ClientIdentityResolver {
    tailscale_peers: Arc<HashMap<IpAddr, ClientIdentity>>,
}

impl ClientIdentityResolver {
    pub fn discover() -> Self {
        match discover_tailscale_status() {
            Ok(output) => match Self::from_tailscale_status(&output) {
                Ok(resolver) => {
                    tracing::info!(
                        addresses = resolver.tailscale_peers.len(),
                        "loaded stable Tailscale client identities"
                    );
                    resolver
                }
                Err(error) => {
                    tracing::warn!(?error, "unable to parse Tailscale client identities");
                    Self::default()
                }
            },
            Err(error) => {
                tracing::info!(
                    ?error,
                    "Tailscale identity discovery is unavailable; trusted clients will bind to their source address"
                );
                Self::default()
            }
        }
    }

    pub fn resolve(&self, ip: IpAddr) -> ClientIdentity {
        let ip = normalize_ip(ip);
        self.tailscale_peers
            .get(&ip)
            .cloned()
            .unwrap_or_else(|| ClientIdentity::address(ip))
    }

    fn from_tailscale_status(output: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            output.len() <= MAX_TAILSCALE_STATUS_BYTES,
            "Tailscale status output is too large"
        );
        let status: TailscaleStatus =
            serde_json::from_slice(output).context("decode tailscale status JSON")?;
        let mut peers = HashMap::new();
        for peer in status.peer.into_values() {
            let stable_id = peer.id.trim();
            if stable_id.is_empty() || stable_id.len() > 128 {
                continue;
            }
            let label = peer.host_name.trim();
            let label = if label.is_empty() || label.len() > 128 {
                stable_id
            } else {
                label
            };
            let identity = ClientIdentity {
                key: format!("tailscale:{stable_id}"),
                label: label.to_string(),
                tailscale: true,
            };
            for address in peer.tailscale_ips {
                if let Ok(ip) = address.parse::<IpAddr>() {
                    peers.insert(normalize_ip(ip), identity.clone());
                }
            }
        }
        Ok(Self {
            tailscale_peers: Arc::new(peers),
        })
    }
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
        ip => ip,
    }
}

fn discover_tailscale_status() -> Result<Vec<u8>> {
    let executable = tailscale_executable().context("tailscale executable not found")?;
    let mut command = Command::new(&executable);
    command.args(["status", "--json"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command
        .output()
        .with_context(|| format!("run {} status --json", executable.display()))?;
    anyhow::ensure!(output.status.success(), "tailscale status failed");
    anyhow::ensure!(
        output.stdout.len() <= MAX_TAILSCALE_STATUS_BYTES,
        "Tailscale status output is too large"
    );
    Ok(output.stdout)
}

fn tailscale_executable() -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Tailscale\tailscale.exe"),
    ];
    #[cfg(not(windows))]
    let candidates = [
        PathBuf::from("/usr/bin/tailscale"),
        PathBuf::from("/usr/local/bin/tailscale"),
    ];

    candidates.into_iter().find(|path| path.is_file())
}

#[derive(Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "Peer", default)]
    peer: BTreeMap<String, TailscalePeer>,
}

#[derive(Deserialize)]
struct TailscalePeer {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RememberedResolution {
    Scale,
    MatchDisplay,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientPreferences {
    pub trusted_username: Option<String>,
    pub resolution: Option<RememberedResolution>,
}

#[derive(Clone)]
pub struct TrustedClientStore {
    inner: Arc<TrustedClientStoreInner>,
}

struct TrustedClientStoreInner {
    path: PathBuf,
    state: Mutex<TrustedClientFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct TrustedClientFile {
    schema_version: u32,
    clients: Vec<TrustedClientRecord>,
}

impl Default for TrustedClientFile {
    fn default() -> Self {
        Self {
            schema_version: TRUST_SCHEMA_VERSION,
            clients: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct TrustedClientRecord {
    identity: String,
    label: String,
    trusted_username: Option<String>,
    resolution: Option<RememberedResolution>,
}

impl TrustedClientStore {
    pub fn load(path: PathBuf) -> Self {
        let state = match load_file(&path) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(?error, path = %path.display(), "trusted-client state is unavailable; no client will bypass authentication");
                TrustedClientFile::default()
            }
        };
        tracing::info!(
            records = state.clients.len(),
            path = %path.display(),
            "loaded trusted-client preferences"
        );
        Self {
            inner: Arc::new(TrustedClientStoreInner {
                path,
                state: Mutex::new(state),
            }),
        }
    }

    pub fn preferences(&self, identity: &ClientIdentity) -> ClientPreferences {
        self.lock_state()
            .clients
            .iter()
            .find(|record| record.identity == identity.key())
            .map_or_else(ClientPreferences::default, |record| ClientPreferences {
                trusted_username: record.trusted_username.clone(),
                resolution: record.resolution,
            })
    }

    pub fn remember_sign_in(&self, identity: &ClientIdentity, username: &str) -> Result<()> {
        let username = username.trim();
        anyhow::ensure!(!username.is_empty(), "trusted username is empty");
        self.update(identity, |record| {
            record.trusted_username = Some(username.to_string());
        })
    }

    pub fn set_resolution(
        &self,
        identity: &ClientIdentity,
        resolution: Option<RememberedResolution>,
    ) -> Result<()> {
        self.update(identity, |record| record.resolution = resolution)
    }

    fn update(
        &self,
        identity: &ClientIdentity,
        update: impl FnOnce(&mut TrustedClientRecord),
    ) -> Result<()> {
        let mut state = self.lock_state();
        let mut next = state.clone();
        let record = match next
            .clients
            .iter_mut()
            .find(|record| record.identity == identity.key())
        {
            Some(record) => record,
            None => {
                next.clients.push(TrustedClientRecord {
                    identity: identity.key().to_string(),
                    label: identity.label().to_string(),
                    ..TrustedClientRecord::default()
                });
                next.clients.last_mut().expect("record was inserted")
            }
        };
        record.label = identity.label().to_string();
        update(record);
        next.clients
            .retain(|record| record.trusted_username.is_some() || record.resolution.is_some());
        persist_file(&self.inner.path, &next)?;
        *state = next;
        Ok(())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, TrustedClientFile> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn load_file(path: &Path) -> Result<TrustedClientFile> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(TrustedClientFile::default());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut state: TrustedClientFile =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    anyhow::ensure!(
        state.schema_version == TRUST_SCHEMA_VERSION,
        "unsupported trusted-client schema {}",
        state.schema_version
    );
    state.clients.retain(|record| {
        !record.identity.trim().is_empty()
            && (record.trusted_username.is_some() || record.resolution.is_some())
    });
    Ok(state)
}

fn persist_file(path: &Path, state: &TrustedClientFile) -> Result<()> {
    let parent = path
        .parent()
        .context("trusted-client state path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let text = toml::to_string_pretty(state).context("serialize trusted-client state")?;
    let temporary = path.with_extension("toml.tmp");
    let backup = path.with_extension("toml.bak");
    fs::write(&temporary, text).with_context(|| format!("write {}", temporary.display()))?;

    if path.exists() {
        let _ = fs::remove_file(&backup);
        fs::rename(path, &backup).with_context(|| format!("back up {}", path.display()))?;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        if backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(error).with_context(|| format!("replace {}", path.display()));
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_ipv4_and_ipv6_resolve_to_the_same_stable_node() {
        let status = br#"{
            "Peer": {
                "nodekey:test": {
                    "ID": "nStable123",
                    "HostName": "jzd-mb0",
                    "TailscaleIPs": ["100.64.33.3", "fd7a:115c:a1e0::4b34:5744"]
                }
            }
        }"#;
        let resolver = ClientIdentityResolver::from_tailscale_status(status).unwrap();
        let ipv4 = resolver.resolve("100.64.33.3".parse().unwrap());
        let ipv6 = resolver.resolve("fd7a:115c:a1e0::4b34:5744".parse().unwrap());
        assert_eq!(ipv4, ipv6);
        assert_eq!(ipv4.key(), "tailscale:nStable123");
        assert_eq!(ipv4.label(), "jzd-mb0");
    }

    #[test]
    fn store_keeps_sign_in_and_resolution_preferences_without_a_password() {
        let root = std::env::temp_dir().join(format!(
            "sunremote-trusted-clients-{}-roundtrip",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("trusted-clients.toml");
        let identity = ClientIdentity::tailscale("nStable123", "jzd-mb0");
        let store = TrustedClientStore::load(path.clone());
        store.remember_sign_in(&identity, "mikoto").unwrap();
        store
            .set_resolution(&identity, Some(RememberedResolution::Scale))
            .unwrap();

        let reloaded = TrustedClientStore::load(path.clone());
        assert_eq!(
            reloaded.preferences(&identity),
            ClientPreferences {
                trusted_username: Some("mikoto".to_string()),
                resolution: Some(RememberedResolution::Scale),
            }
        );
        let serialized = fs::read_to_string(&path).unwrap();
        assert!(!serialized.to_ascii_lowercase().contains("password"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn clearing_resolution_preserves_trusted_sign_in() {
        let root =
            std::env::temp_dir().join(format!("sunremote-clear-resolution-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("trusted-clients.toml");
        let identity = ClientIdentity::tailscale("nStable456", "phone");
        let store = TrustedClientStore::load(path);
        store.remember_sign_in(&identity, "mikoto").unwrap();
        store
            .set_resolution(&identity, Some(RememberedResolution::MatchDisplay))
            .unwrap();
        store.set_resolution(&identity, None).unwrap();

        assert_eq!(
            store.preferences(&identity),
            ClientPreferences {
                trusted_username: Some("mikoto".to_string()),
                resolution: None,
            }
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_persistence_does_not_trust_the_client_in_memory() {
        let root = std::env::temp_dir().join(format!(
            "sunremote-failed-trust-write-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"block").unwrap();
        let store = TrustedClientStore::load(blocker.join("trusted-clients.toml"));
        let identity = ClientIdentity::tailscale("nStable789", "tablet");

        assert!(store.remember_sign_in(&identity, "mikoto").is_err());
        assert_eq!(store.preferences(&identity), ClientPreferences::default());
        let _ = fs::remove_dir_all(root);
    }
}
