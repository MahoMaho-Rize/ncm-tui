use std::{path::Path, sync::Arc, time::Duration};

use rand::{Rng, seq::SliceRandom};
use reqwest::{
    Client, StatusCode,
    cookie::{CookieStore, Jar},
    header::{CONTENT_RANGE, HeaderMap, HeaderValue, RANGE, REFERER, RETRY_AFTER, USER_AGENT},
};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

use super::{
    ApiSpec, CryptoMode, NcmError, PreparedRequest, RawApiResponse, RequestEntropy, Result,
    SessionConfig,
    crypto::{eapi_decrypt, eapi_encrypt, weapi_encrypt},
};

const HOST: &str = "https://music.163.com";
const BASE62: &[u8] = b"PJArHa0dpwhvMNYqKnTbitWfEmosQ9527ZBx46IXUgOzD81VuSFyckLRljG3eC";
const WEAPI_USER_AGENT: &str =
    "Mozilla/5.0 (linux@github.com/mos9527/pyncm_asycn) Chrome/PyNCM_Async.1.8.1";
const EAPI_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 \
(KHTML, like Gecko) Safari/537.36 Chrome/91.0.4472.164 NeteaseMusicDesktop/2.10.2.200154";
const DEFAULT_API_QPS: f64 = 0.0;
const DEFAULT_MAX_RETRIES: usize = 3;

#[derive(Clone)]
pub struct NcmClient {
    http: Client,
    session: SessionConfig,
    _cookie_jar: Arc<Jar>,
    request_policy: Arc<RequestPolicy>,
}

struct RequestPolicy {
    minimum_interval: Duration,
    next_request_at: Mutex<Instant>,
    max_retries: usize,
}

impl NcmClient {
    pub fn new(session: SessionConfig, timeout: Duration) -> Result<Self> {
        Self::with_rate_limit(session, timeout, DEFAULT_API_QPS)
    }

    /// Builds a client whose clones share one API rate limiter and retry budget.
    pub fn with_rate_limit(
        session: SessionConfig,
        timeout: Duration,
        api_qps: f64,
    ) -> Result<Self> {
        let base_url = Url::parse(&format!("{HOST}/"))?;
        let cookie_jar = Arc::new(Jar::default());

        Self::seed_cookie(&cookie_jar, &base_url, "os", &session.os);
        Self::seed_cookie(&cookie_jar, &base_url, "appver", &session.appver);
        Self::seed_cookie(&cookie_jar, &base_url, "osver", &session.osver);
        Self::seed_cookie(&cookie_jar, &base_url, "channel", &session.channel);
        Self::seed_cookie(&cookie_jar, &base_url, "deviceId", &session.device_id);
        if !session.music_u.is_empty() {
            Self::seed_cookie(&cookie_jar, &base_url, "MUSIC_U", &session.music_u);
        }
        if !session.csrf_token.is_empty() {
            Self::seed_cookie(&cookie_jar, &base_url, "__csrf", &session.csrf_token);
        }

        let http = Client::builder()
            .cookie_provider(cookie_jar.clone())
            .timeout(timeout)
            .build()?;

        Ok(Self {
            http,
            session,
            _cookie_jar: cookie_jar,
            request_policy: Arc::new(RequestPolicy {
                minimum_interval: qps_interval(api_qps),
                next_request_at: Mutex::new(Instant::now()),
                max_retries: DEFAULT_MAX_RETRIES,
            }),
        })
    }

    fn seed_cookie(jar: &Jar, url: &Url, name: &str, value: &str) {
        jar.add_cookie_str(
            &format!("{name}={value}; Domain=music.163.com; Path=/"),
            url,
        );
    }

    pub fn session(&self) -> &SessionConfig {
        &self.session
    }

    /// Returns the latest credentials captured by the shared cookie jar.
    pub fn authenticated_session(&self) -> Result<SessionConfig> {
        let url = Url::parse(&format!("{HOST}/"))?;
        let mut session = self.session.clone();
        if let Some(header) = self._cookie_jar.cookies(&url) {
            apply_cookie_header(&mut session, header.to_str().unwrap_or_default());
        }
        Ok(session)
    }

    pub fn prepare(&self, spec: ApiSpec) -> Result<PreparedRequest> {
        self.prepare_with_entropy(spec, RequestEntropy::random())
    }

    pub fn prepare_with_entropy(
        &self,
        mut spec: ApiSpec,
        entropy: RequestEntropy,
    ) -> Result<PreparedRequest> {
        let object = spec
            .payload
            .as_object_mut()
            .ok_or(NcmError::PayloadNotObject)?;

        match spec.crypto {
            CryptoMode::Weapi => {
                object.insert(
                    "csrf_token".to_owned(),
                    Value::String(self.session.csrf_token.clone()),
                );
                let plaintext = serde_json::to_string(&spec.payload)?;
                let encrypted = weapi_encrypt(&plaintext, &entropy.weapi_secret)?;

                Ok(PreparedRequest {
                    method: spec.method,
                    url: format!("{HOST}{}", spec.path.replace("/api/", "/weapi/")),
                    query: vec![("csrf_token".to_owned(), self.session.csrf_token.clone())],
                    headers: weapi_headers()?,
                    form: vec![
                        ("params".to_owned(), encrypted.params),
                        ("encSecKey".to_owned(), encrypted.enc_sec_key),
                    ],
                    crypto: CryptoMode::Weapi,
                })
            }
            CryptoMode::Eapi => {
                let header = json!({
                    "os": self.session.os,
                    "appver": self.session.appver,
                    "osver": self.session.osver,
                    "channel": self.session.channel,
                    "deviceId": self.session.device_id,
                    "requestId": entropy.request_id,
                });
                object.insert(
                    "header".to_owned(),
                    Value::String(serde_json::to_string(&header)?),
                );
                let plaintext = serde_json::to_string(&spec.payload)?;
                let signing_path = spec.path.replacen("/eapi/", "/api/", 1);
                let params = eapi_encrypt(&signing_path, &plaintext)?;

                Ok(PreparedRequest {
                    method: spec.method,
                    url: format!("{HOST}{}", spec.path),
                    query: Vec::new(),
                    headers: eapi_headers()?,
                    form: vec![("params".to_owned(), params)],
                    crypto: CryptoMode::Eapi,
                })
            }
        }
    }

    pub async fn execute_raw(&self, spec: ApiSpec) -> Result<RawApiResponse> {
        for attempt in 0..=self.request_policy.max_retries {
            self.wait_for_request_slot().await;
            let prepared = self.prepare(spec.clone())?;
            let response = self
                .http
                .request(prepared.method, &prepared.url)
                .headers(prepared.headers)
                .query(&prepared.query)
                .form(&prepared.form)
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(error)
                    if attempt < self.request_policy.max_retries
                        && is_retryable_transport(&error) =>
                {
                    tokio::time::sleep(retry_delay(attempt, None)).await;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };

            let status = response.status();
            let headers = response.headers().clone();
            let encrypted_body = match response.bytes().await {
                Ok(body) => body.to_vec(),
                Err(error)
                    if attempt < self.request_policy.max_retries
                        && is_retryable_transport(&error) =>
                {
                    tokio::time::sleep(retry_delay(attempt, None)).await;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };

            if retryable_status(status) && attempt < self.request_policy.max_retries {
                tokio::time::sleep(retry_delay(attempt, retry_after(&headers))).await;
                continue;
            }

            let plaintext_body = match prepared.crypto {
                CryptoMode::Weapi => encrypted_body.clone(),
                CryptoMode::Eapi => match eapi_decrypt(&encrypted_body) {
                    Ok(body) => body,
                    Err(_) if encrypted_body.starts_with(b"{") => encrypted_body.clone(),
                    Err(error) => return Err(error.into()),
                },
            };

            return Ok(RawApiResponse {
                status,
                headers,
                encrypted_body,
                plaintext_body,
            });
        }

        unreachable!("the retry loop always returns on its final attempt")
    }

    pub async fn execute(&self, spec: ApiSpec) -> Result<Value> {
        let response = self.execute_raw(spec).await?;
        if !response.status.is_success() {
            return Err(NcmError::HttpStatus {
                status: response.status,
                body: String::from_utf8_lossy(&response.plaintext_body).into_owned(),
            });
        }
        Ok(serde_json::from_slice(&response.plaintext_body)?)
    }

    /// Streams an audio response to a resumable temporary sibling and atomically installs it.
    pub async fn download_to(&self, url: &str, destination: &Path) -> Result<u64> {
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let partial = destination.with_extension(format!(
            "{}.part",
            destination
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("audio")
        ));
        let offset = tokio::fs::metadata(&partial)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        let request = self.http.get(url);
        let request = if offset > 0 {
            request.header(RANGE, format!("bytes={offset}-"))
        } else {
            request
        };
        let mut response = request.send().await?;
        let mut action = resume_action(
            response.status(),
            offset,
            response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok()),
        );

        if action == ResumeAction::RetryWithoutRange {
            response = self.http.get(url).send().await?;
            action = resume_action(response.status(), 0, None);
        }
        if action == ResumeAction::Complete {
            tokio::fs::rename(&partial, destination).await?;
            return Ok(tokio::fs::metadata(destination).await?.len());
        }
        if action == ResumeAction::Reject {
            return Err(NcmError::DownloadStatus {
                status: response.status(),
                url: url.to_owned(),
            });
        }

        let mut file = match action {
            ResumeAction::Append => {
                tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&partial)
                    .await?
            }
            ResumeAction::Restart => tokio::fs::File::create(&partial).await?,
            ResumeAction::Complete | ResumeAction::RetryWithoutRange | ResumeAction::Reject => {
                unreachable!("terminal resume actions were handled above")
            }
        };
        let mut response = response;
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&partial, destination).await?;
        Ok(tokio::fs::metadata(destination).await?.len())
    }

    /// Fetches a small binary asset, such as album artwork.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self.http.get(url).header(REFERER, HOST).send().await?;
        if !response.status().is_success() {
            return Err(NcmError::DownloadStatus {
                status: response.status(),
                url: url.to_owned(),
            });
        }
        Ok(response.bytes().await?.to_vec())
    }

    async fn wait_for_request_slot(&self) {
        if self.request_policy.minimum_interval.is_zero() {
            return;
        }
        let mut next_request_at = self.request_policy.next_request_at.lock().await;
        let now = Instant::now();
        if *next_request_at > now {
            tokio::time::sleep_until(*next_request_at).await;
        }
        *next_request_at = Instant::now() + self.request_policy.minimum_interval;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResumeAction {
    Append,
    Restart,
    Complete,
    RetryWithoutRange,
    Reject,
}

fn resume_action(status: StatusCode, offset: u64, content_range: Option<&str>) -> ResumeAction {
    match status {
        StatusCode::PARTIAL_CONTENT if offset > 0 => {
            if content_range_start(content_range) == Some(offset) {
                ResumeAction::Append
            } else {
                ResumeAction::RetryWithoutRange
            }
        }
        StatusCode::OK => ResumeAction::Restart,
        StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
            if complete_length(content_range) == Some(offset) {
                ResumeAction::Complete
            } else {
                ResumeAction::RetryWithoutRange
            }
        }
        status if status.is_success() && offset == 0 => ResumeAction::Restart,
        _ => ResumeAction::Reject,
    }
}

fn complete_length(content_range: Option<&str>) -> Option<u64> {
    content_range?.strip_prefix("bytes */")?.parse::<u64>().ok()
}

fn content_range_start(content_range: Option<&str>) -> Option<u64> {
    content_range?
        .strip_prefix("bytes ")?
        .split_once('-')?
        .0
        .parse()
        .ok()
}

fn qps_interval(api_qps: f64) -> Duration {
    if api_qps.is_finite() && api_qps > 0.0 {
        Duration::from_secs_f64(1.0 / api_qps)
    } else {
        Duration::ZERO
    }
}

fn is_retryable_transport(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn retry_delay(attempt: usize, server_delay: Option<Duration>) -> Duration {
    server_delay.unwrap_or_else(|| Duration::from_millis(250 * (1_u64 << attempt.min(3))))
}

fn apply_cookie_header(session: &mut SessionConfig, header: &str) {
    for cookie in header.split(';') {
        let Some((name, value)) = cookie.trim().split_once('=') else {
            continue;
        };
        match name {
            "MUSIC_U" => session.music_u = value.to_owned(),
            "__csrf" => session.csrf_token = value.to_owned(),
            _ => {}
        }
    }
}

impl RequestEntropy {
    pub fn random() -> Self {
        let mut rng = rand::thread_rng();
        let weapi_secret = (0..16)
            .map(|_| *BASE62.choose(&mut rng).expect("BASE62 is not empty") as char)
            .collect();
        Self {
            weapi_secret,
            request_id: rng.gen_range(20_000_000..30_000_000).to_string(),
        }
    }
}

fn weapi_headers() -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(WEAPI_USER_AGENT));
    headers.insert(REFERER, HeaderValue::from_static(HOST));
    Ok(headers)
}

fn eapi_headers() -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(EAPI_USER_AGENT));
    headers.insert(REFERER, HeaderValue::from_static(""));
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ncm_core::api::track;

    #[test]
    fn prepares_eapi_with_distinct_signing_and_transport_paths() {
        let client = NcmClient::new(SessionConfig::default(), Duration::from_secs(30)).unwrap();
        let prepared = client
            .prepare_with_entropy(
                track::audio_v1(&[123], "lossless", "flac"),
                RequestEntropy {
                    weapi_secret: "0123456789abcdef".to_owned(),
                    request_id: "23456789".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(
            prepared.url,
            "https://music.163.com/eapi/song/enhance/player/url/v1"
        );
        assert_eq!(prepared.form[0].0, "params");
        assert!(prepared.form[0].1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn request_policy_handles_invalid_qps_and_bounded_backoff() {
        assert_eq!(qps_interval(DEFAULT_API_QPS), Duration::ZERO);
        assert_eq!(qps_interval(2.0), Duration::from_millis(500));
        assert_eq!(qps_interval(0.0), Duration::ZERO);
        assert_eq!(qps_interval(f64::NAN), Duration::ZERO);
        assert_eq!(retry_delay(0, None), Duration::from_millis(250));
        assert_eq!(retry_delay(20, None), Duration::from_secs(2));
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn extracts_authentication_cookies_without_exposing_the_jar() {
        let mut session = SessionConfig::default();
        apply_cookie_header(
            &mut session,
            "os=pc; MUSIC_U=credential==; __csrf=token; ignored=value",
        );
        assert_eq!(session.music_u, "credential==");
        assert_eq!(session.csrf_token, "token");
    }

    #[test]
    fn chooses_safe_resume_actions() {
        assert_eq!(
            resume_action(StatusCode::PARTIAL_CONTENT, 42, Some("bytes 42-99/100")),
            ResumeAction::Append
        );
        assert_eq!(
            resume_action(StatusCode::PARTIAL_CONTENT, 42, Some("bytes 0-99/100")),
            ResumeAction::RetryWithoutRange
        );
        assert_eq!(
            resume_action(StatusCode::OK, 42, None),
            ResumeAction::Restart
        );
        assert_eq!(
            resume_action(StatusCode::RANGE_NOT_SATISFIABLE, 100, Some("bytes */100")),
            ResumeAction::Complete
        );
        assert_eq!(
            resume_action(StatusCode::RANGE_NOT_SATISFIABLE, 42, Some("bytes */100")),
            ResumeAction::RetryWithoutRange
        );
        assert_eq!(complete_length(Some("invalid")), None);
    }
}
