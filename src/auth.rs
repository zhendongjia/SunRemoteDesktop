use std::path::PathBuf;
use std::{error::Error, fmt};

use anyhow::Result;
use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator, Credentials,
};

use crate::config;

#[derive(Clone)]
pub struct LocalAccountValidator {
    config_path: PathBuf,
}

impl LocalAccountValidator {
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}

#[async_trait::async_trait]
impl CredentialValidator for LocalAccountValidator {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        let config_path = self.config_path.clone();
        let credentials = credentials.clone();
        tokio::task::spawn_blocking(move || validate_blocking(&config_path, &credentials))
            .await
            .map_err(validation_error)?
    }
}

fn validate_blocking(
    config_path: &std::path::Path,
    credentials: &Credentials,
) -> Result<CredentialDecision, CredentialValidationError> {
    let settings = config::load_from(config_path).map_err(validation_error)?;
    let candidates = account_candidates(
        credentials.domain.as_deref().unwrap_or_default(),
        &credentials.username,
    );
    if !settings.allows_user(&candidates) {
        tracing::warn!(user = %credentials.username, "RDP login rejected by allow-list");
        return Ok(CredentialDecision::Reject);
    }

    #[cfg(windows)]
    {
        let valid = crate::platform::windows::validate_local_account(
            credentials.domain.as_deref().unwrap_or_default(),
            &credentials.username,
            &credentials.password,
        )
        .map_err(validation_error)?;
        if valid {
            tracing::info!(user = %credentials.username, "RDP login accepted");
            Ok(CredentialDecision::Accept)
        } else {
            tracing::warn!(user = %credentials.username, "RDP login rejected by Windows");
            Ok(CredentialDecision::Reject)
        }
    }

    #[cfg(not(windows))]
    {
        let _ = credentials;
        Ok(CredentialDecision::Reject)
    }
}

fn account_candidates(domain: &str, username: &str) -> Vec<String> {
    let username = username.trim().to_lowercase();
    let domain = domain.trim().to_lowercase();
    let mut candidates = vec![username.clone()];
    if !domain.is_empty() {
        candidates.push(format!("{domain}\\{username}"));
    }
    if let Ok(computer_name) = std::env::var("COMPUTERNAME") {
        candidates.push(format!("{}\\{username}", computer_name.to_lowercase()));
    }
    candidates.push(format!(".\\{username}"));
    candidates
}

fn validation_error(error: impl fmt::Display) -> CredentialValidationError {
    CredentialValidationError::new(BackendError(error.to_string()))
}

#[derive(Debug)]
struct BackendError(String);

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for BackendError {}
