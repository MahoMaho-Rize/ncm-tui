use ncm_tui::ncm_core::crypto::{eapi_encrypt, weapi_encrypt};

#[test]
fn weapi_matches_vendored_python_implementation() {
    let plaintext = r#"{"c": "[{\"id\": \"123\"}]", "csrf_token": ""}"#;
    let encrypted = weapi_encrypt(plaintext, "0123456789abcdef").unwrap();

    assert_eq!(
        encrypted.params,
        concat!(
            "v9pudnBPYD5CLbPwFQbk2mnmBQDxY8owMINl1KXv3WNI+UxWkT2xHBVmFbNiWZ870wglws0HY3I0\n",
            "UUPE75xKNLxJD0vGxf8JlBg+IN7ZS0s=\n"
        )
    );
    assert_eq!(
        encrypted.enc_sec_key,
        concat!(
            "35701388baf89fed412e11269b9c76625d095ecaf17f03fa018abe19ea2d38b9",
            "49debf242ee39a71ca1f6cda71b1b86a45aa909ee27f7e78e267d34e732f0de9",
            "48206c3340a788d0003372183e2f753c1f78b66ac23d134ac1fc9b993156520ea",
            "826b8aa89a962d4491b4b8d7e08738e1da9b07aa39bf4a7ef0b1c210728cd52"
        )
    );
}

#[test]
fn eapi_matches_vendored_python_implementation() {
    let path = "/api/song/enhance/player/url/v1";
    let plaintext = r#"{"ids": [123], "encodeType": "flac", "level": "lossless", "header": "{\"os\": \"iPhone OS\", \"appver\": \"10.0.0\", \"osver\": \"16.2\", \"channel\": \"distribution\", \"deviceId\": \"pyncm!\", \"requestId\": \"23456789\"}"}"#;

    assert_eq!(
        eapi_encrypt(path, plaintext).unwrap(),
        concat!(
            "fa90b329e9614f79e79598f37dc2edb487f00d1bc4c9b24cd57e6c318b907356",
            "9338432cd7d98d1a3626e997a2c5312110de7a2ee69e593f560e9616a1ff0515",
            "b474ae36eb716df6399e840e484095760b6466962ead4647aa68dba831b7b583",
            "c38ff68c268c875e5c8ab669930a54eae7070b7e46ff346af2686bf706e843d",
            "86947beeea5d7074b3be859da8c8f0908c4c1861fa113bc344a59525c5e0c313",
            "75dc565995d87a6cef47e3a12ba65ae00b886c58d323faf8beda1d6c7f74c637",
            "1066c46254ad629220cada5cdda21ae4c2d0a12a3f7541e86574d684fac33fbc",
            "0ca91417f45f9dccef93bfba7ab5497f78c72db97dbf8710750501c6efdfa777",
            "f85e5a8152a3e360a5f606effa47e3116236a06acaaf46dacd6f90e63e5374d",
            "da3963ae22f5f5dc1a3fc1dd40850b4b2fff3e7713c6251dba9bb9781fdafd0ce5"
        )
    );
}
