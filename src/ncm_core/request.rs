use reqwest::{Method, StatusCode, header::HeaderMap};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CryptoMode {
    Weapi,
    Eapi,
}

#[derive(Clone, Debug)]
pub struct ApiSpec {
    pub method: Method,
    pub path: String,
    pub crypto: CryptoMode,
    pub payload: Value,
}

impl ApiSpec {
    pub fn post(path: impl Into<String>, crypto: CryptoMode, payload: Value) -> Self {
        Self {
            method: Method::POST,
            path: path.into(),
            crypto,
            payload,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RequestEntropy {
    pub weapi_secret: String,
    pub request_id: String,
}

#[derive(Clone, Debug)]
pub struct PreparedRequest {
    pub method: Method,
    pub url: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub form: Vec<(String, String)>,
    pub crypto: CryptoMode,
}

#[derive(Clone, Debug)]
pub struct RawApiResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub encrypted_body: Vec<u8>,
    pub plaintext_body: Vec<u8>,
}
