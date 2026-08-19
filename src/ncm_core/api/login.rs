use serde_json::json;

use crate::ncm_core::{ApiSpec, CryptoMode};

pub fn current_login_status() -> ApiSpec {
    ApiSpec::post("/weapi/w/nuser/account/get", CryptoMode::Weapi, json!({}))
}

pub fn qr_key() -> ApiSpec {
    ApiSpec::post(
        "/weapi/login/qrcode/unikey",
        CryptoMode::Weapi,
        json!({
            "type": "1",
            "noCheckToken": true,
        }),
    )
}

pub fn qr_check(key: &str) -> ApiSpec {
    ApiSpec::post(
        "/weapi/login/qrcode/client/login",
        CryptoMode::Weapi,
        json!({
            "type": 1,
            "noCheckToken": true,
            "key": key,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_requests_match_web_login_contract() {
        let key = qr_key();
        assert_eq!(key.payload["type"], "1");
        assert_eq!(key.payload["noCheckToken"], true);

        let check = qr_check("abc");
        assert_eq!(check.payload["type"], 1);
        assert_eq!(check.payload["key"], "abc");
    }
}
