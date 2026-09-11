use super::tests::CapturingTransport;
use super::tests::DummyAuth;
use super::tests::provider;
use super::*;

#[tokio::test]
async fn model_decode_errors_exclude_response_content_and_offending_values() {
    let bodies: &[&[u8]] = &[
        br#"{"models":"synthetic-private-model-value"}"#,
        br#"{"object":"list","data":[{"id":"synthetic-private-model-value"}]}"#,
        b"{\n  \"synthetic-private-model-value\": ",
        b"{\xffsynthetic-private-model-value}",
    ];
    for body in bodies {
        let transport = CapturingTransport {
            body: Arc::new(body.to_vec()),
            ..Default::default()
        };
        let provider = provider("https://example.invalid/v1");
        let request_url = ModelsClient::<CapturingTransport>::request_url(&provider, "0.154.0");
        let client = ModelsClient::new(transport, provider, Arc::new(DummyAuth));
        let error = client
            .list_models(request_url, HeaderMap::new())
            .await
            .expect_err("malformed rich catalog must remain rejected");
        let ApiError::Stream(message) = error else {
            panic!("expected catalog decode error");
        };
        assert!(message.starts_with("failed to decode models response: "));
        assert!(message.contains(" at line "));
        assert!(message.contains(" column "));
        assert!(!message.contains("synthetic-private-model-value"));
        assert!(!message.contains("body:"));
    }
}
