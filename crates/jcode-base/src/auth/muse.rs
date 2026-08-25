use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MUSE_AUTH_SOURCE_ID: &str = "muse_cli_auth_json";
const MUSE_AUTH_ENV: &str = "MUSE_AUTH_PATH";

#[derive(Debug, Clone)]
pub struct MuseCredentials {
    pub access_token: String,
    /// The LLM|... API key used for https://api.meta.ai/v1 (inference).
    /// The Muse CLI stores this as `providers.meta.api_key`.
    /// For device-code logins this is derived from the dca: access_token via
    /// an internal exchange; for imported CLI creds it is read directly.
    pub api_key: Option<String>,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuseAccount {
    pub label: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JcodeMuseAuthFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub muse_accounts: Vec<MuseAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_muse_account: Option<String>,
}

const ACCOUNT_LABEL_PREFIX: &str = "muse";

pub fn set_active_account_override(label: Option<String>) {
    crate::auth::account_store::set_runtime_active_override(ACCOUNT_LABEL_PREFIX, label);
}

pub fn get_active_account_override() -> Option<String> {
    crate::auth::account_store::runtime_active_override(ACCOUNT_LABEL_PREFIX)
}

pub fn primary_account_label() -> String {
    crate::auth::account_store::canonical_account_label(ACCOUNT_LABEL_PREFIX, 1)
}

pub fn next_account_label() -> Result<String> {
    let auth = load_auth_file()?;
    Ok(crate::auth::account_store::next_account_label(
        ACCOUNT_LABEL_PREFIX,
        auth.muse_accounts.len(),
    ))
}

pub fn login_target_label(requested: Option<&str>) -> Result<String> {
    let auth = load_auth_file()?;
    Ok(crate::auth::account_store::login_target_label(
        ACCOUNT_LABEL_PREFIX,
        requested,
        auth.active_muse_account,
        &auth.muse_accounts,
        |a| a.label.as_str(),
    ))
}

fn relabel_accounts(auth: &mut JcodeMuseAuthFile) -> bool {
    let outcome = crate::auth::account_store::relabel_accounts(
        ACCOUNT_LABEL_PREFIX,
        &mut auth.muse_accounts,
        &mut auth.active_muse_account,
        get_active_account_override(),
        |a| a.label.as_str(),
        |a, label| a.label = label,
    );
    if let Some(label) = outcome.canonical_override_label {
        set_active_account_override(Some(label));
    }
    outcome.changed
}

fn jcode_muse_auth_path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("muse-auth.json"))
}

/// Muse CLI credential path — mirrors the `muse` launcher:
/// `$XDG_CONFIG_HOME/muse/auth.json` else `~/.config/muse/auth.json`,
/// overridable via `MUSE_AUTH_PATH`. Falls back to `~/.jcode/muse-external.json`
/// when the primary is blocked by macOS TCC (the debug binary lacks Full Disk
/// Access for `~/.config/muse`).
pub fn external_muse_auth_path() -> Result<PathBuf> {
    if let Ok(env_path) = std::env::var(MUSE_AUTH_ENV) {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    let primary = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            PathBuf::from(trimmed).join("muse/auth.json")
        } else {
            crate::storage::user_home_path(".config/muse/auth.json")?
        }
    } else {
        crate::storage::user_home_path(".config/muse/auth.json")?
    };
    // TCC fallback: if primary is not readable, try the copy we maintain at ~/.jcode
    if std::fs::metadata(&primary).is_ok() {
        return Ok(primary);
    }
    let fallback = crate::storage::jcode_dir()
        .map(|d| d.join("muse-external.json"))
        .ok();
    if let Some(fb) = fallback {
        if std::fs::metadata(&fb).is_ok() {
            return Ok(fb);
        }
    }
    Ok(primary)
}

pub fn trust_external_auth_for_future_use() -> Result<()> {
    crate::config::Config::allow_external_auth_source_for_path(
        MUSE_AUTH_SOURCE_ID,
        &external_muse_auth_path()?,
    )?;
    super::AuthStatus::invalidate_cache();
    Ok(())
}

pub fn external_auth_allowed() -> bool {
    external_muse_auth_path()
        .ok()
        .map(|path| {
            crate::config::Config::external_auth_source_allowed_for_path(MUSE_AUTH_SOURCE_ID, &path)
        })
        .unwrap_or(false)
}

pub fn external_auth_source_exists() -> bool {
    external_muse_auth_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

pub fn has_unconsented_external_auth() -> Option<PathBuf> {
    let path = external_muse_auth_path().ok()?;
    if path.exists() && !external_auth_allowed() && external_source_has_muse_auth(&path) {
        Some(path)
    } else {
        None
    }
}

fn external_source_has_muse_auth(path: &PathBuf) -> bool {
    load_external_file_has_muse(path).unwrap_or(false)
}

fn load_external_file_has_muse(path: &PathBuf) -> Result<bool> {
    let data = std::fs::read_to_string(path)?;
    let json: serde_json::Value = serde_json::from_str(&data)?;
    let meta = json.get("providers").and_then(|p| p.get("meta"));
    let has_oauth = meta
        .and_then(|m| m.get("mechanism"))
        .and_then(|v| v.as_str())
        .map(|s| s == "oauth")
        .unwrap_or(false);
    let has_api_key = meta
        .and_then(|m| m.get("api_key"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().starts_with("LLM|"))
        .unwrap_or(false);
    let has_access = meta
        .and_then(|m| m.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    Ok(has_oauth && (has_api_key || has_access))
}

/// Load the OAuth token from Muse CLI's `~/.config/muse/auth.json` (consent-gated).
pub fn load_external_muse_credentials() -> Option<MuseCredentials> {
    if !external_auth_allowed() {
        return None;
    }
    let path = external_muse_auth_path().ok()?;
    let data = std::fs::read_to_string(&path).ok()?;
    parse_external_muse_blob(&data)
}

fn parse_external_muse_blob(data: &str) -> Option<MuseCredentials> {
    let json: serde_json::Value = serde_json::from_str(data).ok()?;
    let meta = json.get("providers")?.get("meta")?;
    let mechanism = meta.get("mechanism")?.as_str()?;
    if mechanism != "oauth" {
        return None;
    }
    let access = meta
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let api_key = meta
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("LLM|"));
    // Need at least one of api_key (inference) or access_token (device)
    if api_key.is_none() && access.is_empty() {
        return None;
    }
    let expires_at = meta.get("expires_at").and_then(|v| v.as_i64()).or_else(|| {
        meta.get("expires_at")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
    });
    Some(MuseCredentials {
        access_token: if access.is_empty() {
            api_key.clone().unwrap_or_default()
        } else {
            access
        },
        api_key,
        expires_at,
    })
}

pub fn load_auth_file() -> Result<JcodeMuseAuthFile> {
    let path = jcode_muse_auth_path()?;
    let mut auth = if path.exists() {
        crate::storage::harden_secret_file_permissions(&path);
        crate::storage::read_json(&path)
            .with_context(|| format!("Could not read Muse credentials from {:?}", path))?
    } else {
        JcodeMuseAuthFile::default()
    };
    if relabel_accounts(&mut auth) {
        crate::logging::info("Renaming Muse accounts to animal labels (muse-otter, muse-fox, ...)");
        save_auth_file(&auth)?;
    }
    Ok(auth)
}

pub fn save_auth_file(auth: &JcodeMuseAuthFile) -> Result<()> {
    let path = jcode_muse_auth_path()?;
    let clean = JcodeMuseAuthFile {
        muse_accounts: auth.muse_accounts.clone(),
        active_muse_account: auth.active_muse_account.clone(),
    };
    crate::storage::write_json_secret(&path, &clean)?;
    Ok(())
}

pub fn list_accounts() -> Result<Vec<MuseAccount>> {
    Ok(load_auth_file()?.muse_accounts)
}

pub fn active_account_label() -> Option<String> {
    let auth = load_auth_file().ok()?;
    crate::auth::account_store::active_account_label(
        get_active_account_override(),
        auth.active_muse_account,
        &auth.muse_accounts,
        |a| a.label.as_str(),
    )
}

pub fn set_active_account(label: &str) -> Result<()> {
    let mut auth = load_auth_file()?;
    crate::auth::account_store::set_active_account(
        label,
        &auth.muse_accounts,
        &mut auth.active_muse_account,
        "No Muse account with label '{}' found",
        |a| a.label.as_str(),
    )?;
    save_auth_file(&auth)?;
    set_active_account_override(Some(label.to_string()));
    Ok(())
}

pub fn upsert_account(account: MuseAccount) -> Result<String> {
    let mut auth = load_auth_file()?;
    let label = crate::auth::account_store::upsert_account(
        ACCOUNT_LABEL_PREFIX,
        &mut auth.muse_accounts,
        &mut auth.active_muse_account,
        account,
        |a| a.label.as_str(),
        |a, label| a.label = label,
    );
    save_auth_file(&auth)?;
    Ok(label)
}

pub fn remove_account(label: &str) -> Result<()> {
    let mut auth = load_auth_file()?;
    let before = auth.muse_accounts.len();
    auth.muse_accounts.retain(|a| a.label != label);
    if auth.muse_accounts.len() == before {
        anyhow::bail!("No Muse account with label '{}' found", label);
    }
    if auth.active_muse_account.as_deref() == Some(label) {
        auth.active_muse_account = auth.muse_accounts.first().map(|a| a.label.clone());
    }
    save_auth_file(&auth)?;
    if get_active_account_override().as_deref() == Some(label) {
        set_active_account_override(auth.active_muse_account.clone());
    }
    Ok(())
}

/// True when a jcode-managed Muse credential exists (and optionally not expired).
fn jcode_credentials() -> Option<MuseCredentials> {
    let auth = load_auth_file().ok()?;
    let label = active_account_label().unwrap_or_else(primary_account_label);
    let account = auth
        .muse_accounts
        .iter()
        .find(|a| a.label == label)
        .or_else(|| auth.muse_accounts.first())?;
    Some(MuseCredentials {
        access_token: account.access_token.clone(),
        api_key: account.api_key.clone(),
        expires_at: account.expires_at,
    })
}

fn is_expired(creds: &MuseCredentials) -> bool {
    if let Some(exp) = creds.expires_at {
        // Muse launcher stores expires_at as epoch seconds? Accept both s and ms.
        let exp_ms = if exp < 10_000_000_000 {
            exp * 1000
        } else {
            exp
        };
        let now = chrono::Utc::now().timestamp_millis();
        exp_ms <= now + 60_000
    } else {
        false
    }
}

pub fn has_muse_credentials() -> bool {
    if let Some(c) = jcode_credentials() {
        let has_key = c
            .api_key
            .as_deref()
            .map(|k| k.trim().starts_with("LLM|"))
            .unwrap_or(false)
            || !c.access_token.trim().is_empty();
        if has_key && !is_expired(&c) {
            return true;
        }
    }
    if let Some(c) = load_external_muse_credentials() {
        let has_key = c
            .api_key
            .as_deref()
            .map(|k| k.trim().starts_with("LLM|"))
            .unwrap_or(false)
            || !c.access_token.trim().is_empty();
        if has_key && !is_expired(&c) {
            return true;
        }
    }
    false
}

pub fn has_muse_credentials_fast() -> bool {
    // Fast path avoids parsing external file twice; same as above but cheap.
    has_muse_credentials()
}

pub fn load_credentials() -> Result<MuseCredentials> {
    if let Some(c) = jcode_credentials() {
        let has_key = c
            .api_key
            .as_deref()
            .map(|k| k.trim().starts_with("LLM|"))
            .unwrap_or(false)
            || !c.access_token.trim().is_empty();
        if has_key {
            if is_expired(&c) {
                anyhow::bail!(
                    "Muse OAuth token is expired. Run `jcode login --provider muse` to refresh."
                );
            }
            return Ok(c);
        }
    }
    if let Some(c) = load_external_muse_credentials() {
        if is_expired(&c) {
            anyhow::bail!(
                "Muse CLI OAuth token is expired. Run `muse login` then `jcode login --provider muse` or re-approve the external source."
            );
        }
        return Ok(c);
    }
    anyhow::bail!(
        "No Muse OAuth credentials found. Run `jcode login --provider muse` or `muse login` then approve the import."
    );
}

pub fn load_credentials_for_account(label: &str) -> Result<MuseCredentials> {
    let auth = load_auth_file()?;
    let account = auth
        .muse_accounts
        .iter()
        .find(|a| a.label == label)
        .ok_or_else(|| anyhow::anyhow!("No Muse account with label '{}' found", label))?;
    Ok(MuseCredentials {
        access_token: account.access_token.clone(),
        api_key: account.api_key.clone(),
        expires_at: account.expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_external_muse_blob_ok() {
        let data = r#"{"providers":{"meta":{"mechanism":"oauth","access_token":"tok123","expires_at":9999999999999}}}"#;
        let c = parse_external_muse_blob(data).unwrap();
        assert_eq!(c.access_token, "tok123");
    }

    #[test]
    fn parse_external_muse_blob_rejects_api() {
        let data = r#"{"providers":{"meta":{"mechanism":"api_key","access_token":"tok"}}}"#;
        assert!(parse_external_muse_blob(data).is_none());
    }
}
