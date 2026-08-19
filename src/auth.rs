//! Authentication domain service with resumable QR polling and private session persistence.

use std::{path::PathBuf, time::Duration};

use serde_json::Value;
use thiserror::Error;

use crate::ncm_core::{NcmClient, NcmError, SessionConfig, api};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QrChallenge {
    pub key: String,
    pub url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub user_id: u64,
    pub nickname: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QrStatus {
    Waiting,
    Scanned,
    Authenticated(Identity),
    Expired,
    RiskControlled,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error(transparent)]
    Api(#[from] NcmError),
    #[error("NCM response is missing {0}")]
    InvalidResponse(&'static str),
    #[error("NCM rejected authentication with code {code}: {message}")]
    Rejected { code: i64, message: String },
}

pub type Result<T> = std::result::Result<T, AuthError>;

#[derive(Clone)]
pub struct Authentication {
    client: NcmClient,
    session_file: PathBuf,
}

impl Authentication {
    pub fn new(client: NcmClient, session_file: impl Into<PathBuf>) -> Self {
        Self {
            client,
            session_file: session_file.into(),
        }
    }

    pub fn load_session(&self) -> Result<SessionConfig> {
        Ok(SessionConfig::load(&self.session_file)?)
    }

    pub async fn current_identity(&self) -> Result<Identity> {
        let response = self
            .client
            .execute(api::login::current_login_status())
            .await?;
        identity_from_response(&response)
    }

    pub async fn begin_qr(&self) -> Result<QrChallenge> {
        let response = self.client.execute(api::login::qr_key()).await?;
        ensure_code(&response, 200)?;
        let key = response
            .get("unikey")
            .or_else(|| response.get("data").and_then(|data| data.get("unikey")))
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or(AuthError::InvalidResponse("unikey"))?
            .to_owned();
        Ok(QrChallenge {
            url: format!("https://music.163.com/login?codekey={key}"),
            key,
        })
    }

    /// Polls once so a TUI can remain responsive and control its own cadence.
    pub async fn poll_qr(&self, challenge: &QrChallenge) -> Result<QrStatus> {
        let response = self
            .client
            .execute(api::login::qr_check(&challenge.key))
            .await?;
        let code = response
            .get("code")
            .and_then(Value::as_i64)
            .ok_or(AuthError::InvalidResponse("code"))?;
        match code {
            800 => Ok(QrStatus::Expired),
            801 => Ok(QrStatus::Waiting),
            802 => Ok(QrStatus::Scanned),
            803 => {
                let identity = self.current_identity().await?;
                self.client
                    .authenticated_session()?
                    .save(&self.session_file)?;
                Ok(QrStatus::Authenticated(identity))
            }
            8821 => Ok(QrStatus::RiskControlled),
            code => Err(AuthError::Rejected {
                code,
                message: message_at(&response),
            }),
        }
    }

    /// Convenience flow for non-interactive frontends. TUI code should use begin/poll.
    pub async fn wait_for_qr(
        &self,
        challenge: &QrChallenge,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<QrStatus> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let status = self.poll_qr(challenge).await?;
            if !matches!(status, QrStatus::Waiting | QrStatus::Scanned) {
                return Ok(status);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(QrStatus::Expired);
            }
            tokio::time::sleep(poll_interval).await;
        }
    }
}

fn identity_from_response(response: &Value) -> Result<Identity> {
    ensure_code(response, 200)?;
    let account = response.get("account");
    let profile = response.get("profile");
    let user_id = account
        .and_then(|value| value.get("id"))
        .and_then(number)
        .or_else(|| {
            profile
                .and_then(|value| value.get("userId"))
                .and_then(number)
        })
        .filter(|id| *id > 0)
        .ok_or(AuthError::InvalidResponse("account.id/profile.userId"))?;
    let nickname = profile
        .and_then(|value| value.get("nickname"))
        .and_then(Value::as_str)
        .unwrap_or("Unknown user")
        .to_owned();
    Ok(Identity { user_id, nickname })
}

fn ensure_code(response: &Value, expected: i64) -> Result<()> {
    let code = response
        .get("code")
        .and_then(Value::as_i64)
        .ok_or(AuthError::InvalidResponse("code"))?;
    if code == expected {
        Ok(())
    } else {
        Err(AuthError::Rejected {
            code,
            message: message_at(response),
        })
    }
}

fn message_at(response: &Value) -> String {
    response
        .get("message")
        .or_else(|| response.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("unknown authentication error")
        .to_owned()
}

fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_login_identity_from_account_and_profile() {
        let identity = identity_from_response(&json!({
            "code": 200,
            "account": { "id": 7 },
            "profile": { "nickname": "NCM user" }
        }))
        .unwrap();
        assert_eq!(identity.user_id, 7);
        assert_eq!(identity.nickname, "NCM user");
    }

    #[test]
    fn rejects_success_without_an_identity() {
        let error = identity_from_response(&json!({ "code": 200 })).unwrap_err();
        assert!(matches!(error, AuthError::InvalidResponse(_)));
    }
}
