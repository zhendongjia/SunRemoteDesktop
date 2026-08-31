use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const PREFERENCE_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResolutionPreference {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Default)]
pub(crate) struct ResolutionPreferenceStore {
    inner: Arc<ResolutionPreferenceStoreInner>,
}

#[derive(Default)]
struct ResolutionPreferenceStoreInner {
    path: Option<PathBuf>,
    state: Mutex<PreferenceFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct PreferenceFile {
    schema_version: u32,
    resolution_by_account: BTreeMap<String, ResolutionPreference>,
}

impl Default for PreferenceFile {
    fn default() -> Self {
        Self {
            schema_version: PREFERENCE_SCHEMA_VERSION,
            resolution_by_account: BTreeMap::new(),
        }
    }
}

impl ResolutionPreferenceStore {
    pub(crate) fn load(path: PathBuf) -> Self {
        let state = match load_file(&path) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    path = %path.display(),
                    "display preferences are unavailable; no saved choice will be applied"
                );
                PreferenceFile::default()
            }
        };
        tracing::info!(
            records = state.resolution_by_account.len(),
            path = %path.display(),
            "loaded account-scoped display preferences"
        );
        Self {
            inner: Arc::new(ResolutionPreferenceStoreInner {
                path: Some(path),
                state: Mutex::new(state),
            }),
        }
    }

    pub(crate) fn get(&self, account: &str) -> Option<ResolutionPreference> {
        let account = normalize_account(account);
        self.lock_state()
            .resolution_by_account
            .get(&account)
            .copied()
    }

    pub(crate) fn set(
        &self,
        account: &str,
        preference: Option<ResolutionPreference>,
    ) -> Result<()> {
        let account = normalize_account(account);
        ensure!(!account.is_empty(), "display-preference account is empty");

        let mut state = self.lock_state();
        let mut next = state.clone();
        match preference {
            Some(preference) => {
                next.resolution_by_account.insert(account, preference);
            }
            None => {
                next.resolution_by_account.remove(&account);
            }
        }
        if let Some(path) = self.inner.path.as_deref() {
            persist_file(path, &next)?;
        }
        *state = next;
        Ok(())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PreferenceFile> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn normalize_account(account: &str) -> String {
    account.trim().to_lowercase()
}

fn load_file(path: &Path) -> Result<PreferenceFile> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(PreferenceFile::default()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut state: PreferenceFile =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        state.schema_version == PREFERENCE_SCHEMA_VERSION,
        "unsupported display-preference schema {}",
        state.schema_version
    );
    state
        .resolution_by_account
        .retain(|account, _| !account.trim().is_empty());
    Ok(state)
}

fn persist_file(path: &Path, state: &PreferenceFile) -> Result<()> {
    let parent = path
        .parent()
        .context("display-preference path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let text = toml::to_string_pretty(state).context("serialize display preferences")?;
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
    fn preferences_are_account_scoped_and_contain_no_credentials() {
        let root = std::env::temp_dir().join(format!(
            "sunremote-display-preferences-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("preferences.toml");
        let store = ResolutionPreferenceStore::load(path.clone());
        store
            .set(
                "HOST\\Alice",
                Some(ResolutionPreference {
                    width: 1920,
                    height: 1200,
                }),
            )
            .unwrap();
        store
            .set(
                "HOST\\Bob",
                Some(ResolutionPreference {
                    width: 1280,
                    height: 800,
                }),
            )
            .unwrap();

        let reloaded = ResolutionPreferenceStore::load(path.clone());
        assert_eq!(
            reloaded.get("host\\ALICE"),
            Some(ResolutionPreference {
                width: 1920,
                height: 1200,
            })
        );
        assert_eq!(
            reloaded.get("host\\bob"),
            Some(ResolutionPreference {
                width: 1280,
                height: 800,
            })
        );
        let serialized = fs::read_to_string(path).unwrap();
        assert!(!serialized.to_lowercase().contains("password"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_write_does_not_change_the_in_memory_preference() {
        let root = std::env::temp_dir().join(format!(
            "sunremote-display-preference-failure-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"block").unwrap();
        let store = ResolutionPreferenceStore::load(blocker.join("preferences.toml"));

        assert!(
            store
                .set(
                    "alice",
                    Some(ResolutionPreference {
                        width: 1920,
                        height: 1080,
                    }),
                )
                .is_err()
        );
        assert_eq!(store.get("alice"), None);
        let _ = fs::remove_dir_all(root);
    }
}
