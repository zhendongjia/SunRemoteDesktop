use std::path::PathBuf;

use anyhow::{Context, Result};
use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator, Credentials,
};
use zeroize::Zeroizing;

use crate::access::AccessGate;
use crate::config;

#[derive(Clone)]
pub struct LocalAccountValidator {
    config_path: PathBuf,
    access_gate: AccessGate,
}

impl LocalAccountValidator {
    pub fn new(config_path: PathBuf, access_gate: AccessGate) -> Self {
        Self {
            config_path,
            access_gate,
        }
    }
}

#[async_trait::async_trait]
impl CredentialValidator for LocalAccountValidator {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        // NLA/CredSSP authenticates before the RDP ClientInfo PDU exists. The
        // synthetic TLS continuation therefore carries no password and must
        // not reopen the in-session login screen.
        if self.access_gate.is_authenticated() {
            return Ok(CredentialDecision::Accept);
        }

        if credentials.username.trim().is_empty() || credentials.password.is_empty() {
            self.access_gate.show_login();
            tracing::info!("RDP client did not provide credentials; showing SunRDP access screen");
            return Ok(CredentialDecision::Accept);
        }

        let config_path = self.config_path.clone();
        let credentials = credentials.clone();
        let username = display_account(&credentials);
        let generation = self.access_gate.begin_validation(&username);
        let result = tokio::task::spawn_blocking(move || {
            let password = Zeroizing::new(credentials.password);
            verify_credentials(
                &config_path,
                credentials.domain.as_deref().unwrap_or_default(),
                &credentials.username,
                &password,
            )
        })
        .await
        .context("join local account validation task")
        .and_then(|result| result);
        self.access_gate
            .finish_validation(generation, &username, result);

        // Invalid or unavailable ClientInfo credentials are routed to the
        // SunRDP access screen. The real desktop remains gated until the local
        // account verifier succeeds.
        Ok(CredentialDecision::Accept)
    }
}

pub fn verify_account(
    config_path: &std::path::Path,
    account: &str,
    password: &str,
) -> Result<bool> {
    let (domain, username) = split_account(account);
    verify_credentials(config_path, domain, username, password)
}

pub(crate) fn is_account_allowed(config_path: &std::path::Path, account: &str) -> Result<bool> {
    let settings = config::load_from(config_path)?;
    let (domain, username) = split_account(account);
    Ok(settings.allows_user(&account_candidates(domain, username)))
}

fn verify_credentials(
    config_path: &std::path::Path,
    domain: &str,
    username: &str,
    password: &str,
) -> Result<bool> {
    let settings = config::load_from(config_path)?;
    let candidates = account_candidates(domain, username);
    if !settings.allows_user(&candidates) {
        tracing::warn!(user = %username, "SunRDP login rejected by allow-list");
        return Ok(false);
    }

    #[cfg(windows)]
    {
        let valid = crate::platform::windows::validate_local_account(domain, username, password)?;
        if valid {
            tracing::info!(user = %username, "SunRDP local account accepted");
            Ok(true)
        } else {
            tracing::warn!(user = %username, "SunRDP login rejected by Windows");
            Ok(false)
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (domain, username, password);
        Ok(false)
    }
}

fn split_account(account: &str) -> (&str, &str) {
    account
        .trim()
        .rsplit_once('\\')
        .map_or(("", account.trim()), |(domain, username)| {
            (domain.trim(), username.trim())
        })
}

fn display_account(credentials: &Credentials) -> String {
    match credentials.domain.as_deref().map(str::trim) {
        Some(domain) if !domain.is_empty() => format!("{domain}\\{}", credentials.username),
        _ => credentials.username.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_local_and_qualified_accounts() {
        assert_eq!(split_account("alice"), ("", "alice"));
        assert_eq!(split_account(".\\alice"), (".", "alice"));
        assert_eq!(split_account("HOST\\alice"), ("HOST", "alice"));
    }
}
