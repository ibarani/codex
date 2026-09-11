use super::*;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::cache::ModelsCacheEntry;
use codex_models_manager::cache::ModelsCacheError;
use codex_models_manager::cache::ModelsCacheFuture;
use codex_models_manager::manager::RefreshStrategy;
use pretty_assertions::assert_eq;

#[derive(Debug)]
struct UntouchedCache;

impl ModelsCache for UntouchedCache {
    fn load<'a>(
        &'a self,
        _client_version: &'a str,
    ) -> ModelsCacheFuture<'a, Result<Option<ModelsCacheEntry>, ModelsCacheError>> {
        panic!("disabled provider must not load caller cache")
    }

    fn store<'a>(
        &'a self,
        _entry: &'a ModelsCacheEntry,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>> {
        panic!("disabled provider must not store caller cache")
    }

    fn refresh_ttl<'a>(
        &'a self,
        _client_version: &'a str,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>> {
        panic!("disabled provider must not renew caller cache")
    }
}

#[tokio::test]
async fn parsed_disabled_provider_reaches_all_manager_factories() {
    let info: ModelProviderInfo =
        serde_json::from_str(r#"{"name":"Custom","model_discovery":"disabled"}"#)
            .expect("typed provider config");
    let provider = create_model_provider(info, /*auth_manager*/ None);
    let home = std::env::temp_dir().join(format!(
        "codex-disabled-model-catalog-{}",
        std::process::id()
    ));
    assert!(
        !home.exists(),
        "test uses an absent directory and never creates it"
    );
    let managers = [
        provider.models_manager(home.clone(), /*config_model_catalog*/ None),
        provider.models_manager_without_cache(/*config_model_catalog*/ None),
        provider.models_manager_with_cache(
            /*config_model_catalog*/ None,
            Arc::new(UntouchedCache),
        ),
    ];
    let config = ModelsManagerConfig {
        model_context_window: Some(1_048_576),
        ..Default::default()
    };
    let http = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    for manager in managers {
        let expected = manager.get_model_info("kimi-k3", &config).await;
        for strategy in [
            RefreshStrategy::Offline,
            RefreshStrategy::OnlineIfUncached,
            RefreshStrategy::Online,
        ] {
            manager.raw_model_catalog(strategy, http.clone()).await;
        }
        manager
            .refresh_if_new_etag("new".to_string(), http.clone())
            .await;
        assert_eq!(manager.get_model_info("kimi-k3", &config).await, expected);
        for allow_fallback in [false, true] {
            assert_eq!(
                manager
                    .get_default_model(
                        &Some("kimi-k3".to_string()),
                        allow_fallback,
                        RefreshStrategy::Online,
                        http.clone()
                    )
                    .await,
                "kimi-k3"
            );
        }
    }
    assert!(!home.exists());
}
