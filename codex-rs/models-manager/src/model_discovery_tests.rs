use super::*;
use pretty_assertions::assert_eq;

#[derive(Debug)]
struct DiscoveryDisabled;

impl ModelsEndpointClient for DiscoveryDisabled {
    fn remote_models_enabled(&self) -> bool {
        false
    }

    fn has_command_auth(&self) -> bool {
        panic!("disabled discovery must not inspect auth")
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        panic!("disabled discovery must not acquire auth")
    }

    fn list_models<'a>(
        &'a self,
        _client_version: &'a str,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<(Vec<ModelInfo>, Option<String>)>> {
        panic!("disabled discovery must not call the endpoint")
    }
}

#[derive(Debug)]
struct UntouchedCache;

impl ModelsCache for UntouchedCache {
    fn load<'a>(
        &'a self,
        _client_version: &'a str,
    ) -> ModelsCacheFuture<'a, Result<Option<ModelsCacheEntry>, ModelsCacheError>> {
        panic!("disabled discovery must not load cache")
    }

    fn store<'a>(
        &'a self,
        _entry: &'a ModelsCacheEntry,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>> {
        panic!("disabled discovery must not store cache")
    }

    fn refresh_ttl<'a>(
        &'a self,
        _client_version: &'a str,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>> {
        panic!("disabled discovery must not refresh cache TTL")
    }
}

#[tokio::test]
async fn disabled_discovery_bypasses_auth_endpoint_and_all_cache_operations() {
    let manager = OpenAiModelsManager::new_with_cache(
        Arc::new(UntouchedCache),
        Arc::new(DiscoveryDisabled),
        /*auth_manager*/ None,
    );
    let before = manager.get_remote_models().await;
    for strategy in [
        RefreshStrategy::Offline,
        RefreshStrategy::OnlineIfUncached,
        RefreshStrategy::Online,
    ] {
        manager
            .refresh_available_models(strategy, &DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .expect("disabled refresh succeeds");
    }
    *manager.etag.write().await = Some("existing".to_string());
    for etag in ["existing", "different"] {
        manager
            .refresh_if_new_etag(etag.to_string(), DEFAULT_HTTP_CLIENT_FACTORY)
            .await;
    }
    assert_eq!(manager.get_etag().await, Some("existing".to_string()));
    assert_eq!(manager.get_remote_models().await, before);
}

#[tokio::test]
async fn disabled_discovery_ignores_poisoned_file_cache_and_preserves_model_metadata() {
    let home = tempdir().expect("private cache directory");
    let cache_path = home.path().join(MODEL_CACHE_FILE);
    let poison = ModelsCacheEntry {
        fetched_at: Utc::now(),
        etag: Some("other-provider".to_string()),
        client_version: Some(crate::client_version_to_whole()),
        models: vec![remote_model(
            "kimi-k3",
            "Unrelated provider",
            /*priority*/ 0,
        )],
    };
    let cache_bytes = serde_json::to_vec(&poison).expect("serialize seeded cache");
    std::fs::write(&cache_path, &cache_bytes).expect("seed private cache");
    let manager = OpenAiModelsManager::new(
        home.path().to_path_buf(),
        Arc::new(DiscoveryDisabled),
        /*auth_manager*/ None,
    );
    let config = ModelsManagerConfig {
        model_context_window: Some(1_048_576),
        ..Default::default()
    };
    for slug in ["kimi-k3", "second-unknown-provider-model"] {
        let expected = manager.get_model_info(slug, &config).await;
        for strategy in [
            RefreshStrategy::Offline,
            RefreshStrategy::OnlineIfUncached,
            RefreshStrategy::Online,
        ] {
            manager
                .refresh_available_models(strategy, &DEFAULT_HTTP_CLIENT_FACTORY)
                .await
                .expect("disabled refresh succeeds");
        }
        let actual = manager.get_model_info(slug, &config).await;
        assert_eq!(actual, expected);
        assert_eq!(actual.context_window, Some(1_048_576));
        assert_eq!(actual.max_context_window, None);
        assert!(actual.used_fallback_model_metadata);
        assert!(
            !actual
                .get_model_instructions(/*personality*/ None)
                .is_empty()
        );
        let requested = Some(slug.to_string());
        for allow_fallback in [false, true] {
            assert_eq!(
                manager
                    .get_default_model(
                        &requested,
                        allow_fallback,
                        RefreshStrategy::Online,
                        DEFAULT_HTTP_CLIENT_FACTORY
                    )
                    .await,
                slug
            );
        }
    }
    assert_eq!(
        std::fs::read(cache_path).expect("read unchanged cache"),
        cache_bytes
    );
}
