use serde_json::json;

use crate::ncm_core::{ApiSpec, CryptoMode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchType {
    Song,
    Album,
    Artist,
    Playlist,
}

impl SearchType {
    pub const fn code(self) -> u16 {
        match self {
            Self::Song => 1,
            Self::Album => 10,
            Self::Artist => 100,
            Self::Playlist => 1000,
        }
    }
}

pub fn cloud(keyword: &str, search_type: SearchType, limit: usize, offset: usize) -> ApiSpec {
    ApiSpec::post(
        "/eapi/cloudsearch/pc",
        CryptoMode::Eapi,
        json!({
            "s": keyword,
            "type": search_type.code().to_string(),
            "limit": limit.to_string(),
            "offset": offset.to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_search_uses_desktop_eapi_contract() {
        let spec = cloud("Vocaloid", SearchType::Artist, 50, 100);
        assert_eq!(spec.path, "/eapi/cloudsearch/pc");
        assert_eq!(spec.crypto, CryptoMode::Eapi);
        assert_eq!(spec.payload["type"], "100");
        assert_eq!(spec.payload["offset"], "100");
    }
}
