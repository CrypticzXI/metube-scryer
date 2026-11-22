use scryer_plugin_pdk::{FnResult, component, config};
use scryer_plugin_sdk::{host::PluginHttpRequest, *};
use std::collections::BTreeMap;

mod metadata;
mod sources;

#[cfg(target_arch = "wasm32")]
scryer_plugin_pdk::scryer_indexer_component_main!(descriptor = descriptor, search = search,);

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "youtube-library".into(),
        name: "YouTube Library".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        sdk_version: SDK_VERSION.into(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        settings: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "youtube-library".into(),
            provider_aliases: vec![],
            provider_profiles: vec![],
            search_semantics_version: None,
            strategy_plan: Some(IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 1,
            }),
            source_kind: IndexerSourceKind::Http,
            capabilities: IndexerCapabilities {
                rss: false,
                search: true,
                // ConfigFieldRole::ConnectionUrl grants the operator-configured
                // origin. Dynamic endpoint grants are reserved for Cardigann.
                resolve_network_endpoints: false,
                query_param: Some("q".into()),
                supported_query_facets: vec!["movie".into(), "series".into(), "anime".into()],
                protocols: vec![IndexerProtocol::Http],
                feed_modes: vec![IndexerFeedMode::AutomaticSearch, IndexerFeedMode::InteractiveSearch],
                search_inputs: vec![
                    IndexerSearchInput::TextQuery,
                    IndexerSearchInput::TitleQuery,
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::AirDate,
                    IndexerSearchInput::SpecialEpisodeTitle,
                    IndexerSearchInput::Limit,
                ],
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(20),
                    max_page_size: Some(20),
                    paged_search: false,
                    rate_limit_hint_seconds: Some(2),
                    ..Default::default()
                }),
                http: Some(IndexerHttpCapabilities {
                    transports: vec![HttpTransport::ProviderPage],
                    providers: vec!["youtube".into()],
                }),
                ..Default::default()
            },
            scoring_policies: vec![],
            config_fields: vec![ConfigFieldDef {
                key: "base_url".into(),
                label: "Indexer Service URL".into(),
                field_type: ConfigFieldType::String,
                required: true,
                role: Some(ConfigFieldRole::ConnectionUrl),
                default_value: Some("http://127.0.0.1:8081".into()),
                help_text: Some(
                    "The address of your YouTube indexer service, reachable from Scryer.".into(),
                ),
                ..Default::default()
            }, ConfigFieldDef {
                key: "source_mappings".into(),
                label: "Official series sources (JSON)".into(),
                field_type: ConfigFieldType::String,
                required: false,
                help_text: Some("Optional JSON array of {series_name,scryer_title_id?,tvdb_id?,channel_url}. Link each exact Scryer series to its official YouTube channel or playlist. Required for automatic episode matching; at most 100 sources. Source order never determines episode numbers.".into()),
                ..Default::default()
            }, ConfigFieldDef {
                key: "scryer_url".into(),
                label: "Scryer URL for episode metadata".into(),
                field_type: ConfigFieldType::String,
                required: false,
                help_text: Some("Optional Scryer address reachable from the indexer service, e.g. http://127.0.0.1:8080. Enables verified episode matching. Leave all Scryer metadata fields blank for ordinary interactive searches. A URL without a password uses Scryer's open-admin mode.".into()),
                ..Default::default()
            }, ConfigFieldDef {
                key: "scryer_username".into(),
                label: "Scryer username".into(),
                field_type: ConfigFieldType::String,
                required: false,
                help_text: Some("Your existing Scryer login; defaults to admin when a password is supplied. These credentials let the indexer service read episode metadata, and are not a separate indexer service access token.".into()),
                ..Default::default()
            }, ConfigFieldDef {
                key: "scryer_password".into(),
                label: "Scryer password".into(),
                field_type: ConfigFieldType::Password,
                required: false,
                help_text: Some("Supply your existing Scryer password only when Scryer requires login. With a password, the Scryer URL defaults to http://127.0.0.1:8080 and username to admin. Leave blank for open-admin mode; Scryer stores password fields encrypted.".into()),
                ..Default::default()
            }],
            allowed_hosts: vec![],
            rate_limit_seconds: Some(2),
        }),
    }
}
fn plugin_error(code: PluginErrorCode, message: &str) -> scryer_plugin_pdk::Error {
    component::structured_plugin_error(PluginError {
        code,
        public_message: message.into(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    })
}
fn base_url() -> FnResult<String> {
    let base = config::get("base_url")?.ok_or_else(|| {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            "Indexer Service URL is required",
        )
    })?;
    let parsed = url::Url::parse(&base).map_err(|_| {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            "Indexer Service URL must be an HTTP(S) address without credentials",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "Indexer Service URL must be an HTTP(S) address without credentials",
        ));
    }
    Ok(base.trim_end_matches('/').into())
}
pub async fn search(request: PluginSearchRequest) -> FnResult<PluginSearchResponse> {
    let base = base_url()?;
    // Scryer uses a minimal/empty search for its connection probe. Verify the
    // indexer service rather than returning an empty list without contacting it.
    let probing = is_connection_probe(&request);
    let response = component::http(PluginHttpRequest {
        url: format!(
            "{base}/api/{}",
            if probing { "status" } else { "scryer/search" }
        ),
        method: Some(if probing { "GET" } else { "POST" }.into()),
        headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
        body: if probing {
            vec![]
        } else {
            let url = config::get("scryer_url")?;
            let username = config::get("scryer_username")?;
            let password = config::get("scryer_password")?;
            let connection =
                metadata::connection(url.as_deref(), username.as_deref(), password.as_deref())
                    .map_err(|message| plugin_error(PluginErrorCode::InvalidConfig, message))?;
            let raw_sources = config::get("source_mappings")?;
            let sources = sources::parse(raw_sources.as_deref())
                .map_err(|message| plugin_error(PluginErrorCode::InvalidConfig, message))?;
            metadata::search_body(&request, connection, sources)?
        },
    })
    .await
    .map_err(|_| {
        plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            "Cannot reach the YouTube indexer service",
        )
    })?;
    if response.status != 200 {
        return Err(plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            "Indexer service search failed",
        ));
    }
    if probing {
        validate_service_status(&response.body)?;
        return Ok(PluginSearchResponse::default());
    }
    let result: PluginResult<PluginSearchResponse> = serde_json::from_slice(&response.body)?;
    match result {
        PluginResult::Ok(r) => Ok(r),
        PluginResult::Err(e) => Err(component::structured_plugin_error(e)),
    }
}
fn is_connection_probe(request: &PluginSearchRequest) -> bool {
    request.query.trim().is_empty()
        && request.ids.is_empty()
        && request.season.is_none()
        && request.episode.is_none()
        && request.absolute_episode.is_none()
        && request
            .context
            .as_ref()
            .is_none_or(|context| context.air_date.is_none() && context.episode_title.is_none())
}
fn validate_service_status(body: &[u8]) -> FnResult<()> {
    let status: serde_json::Value = serde_json::from_slice(body)?;
    if status
        .get("yt_dlp")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "The indexer service is reachable, but yt-dlp is unavailable. Check its runtime installation.",
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supports_verified_automatic_episode_search_without_rss() {
        let d = descriptor();
        let p = d.indexer().unwrap();
        assert_eq!(p.config_fields.len(), 5);
        assert_eq!(p.config_fields[0].key, "base_url");
        assert!(!p.capabilities.rss);
        assert!(
            !p.capabilities.resolve_network_endpoints,
            "Third-party plugins cannot request Cardigann-only endpoint resolution"
        );
        assert_eq!(p.capabilities.protocols, vec![IndexerProtocol::Http]);
        assert_eq!(
            p.capabilities.feed_modes,
            vec![
                IndexerFeedMode::AutomaticSearch,
                IndexerFeedMode::InteractiveSearch
            ]
        );
        for input in [
            IndexerSearchInput::Season,
            IndexerSearchInput::Episode,
            IndexerSearchInput::AirDate,
            IndexerSearchInput::SpecialEpisodeTitle,
        ] {
            assert!(p.capabilities.search_inputs.contains(&input));
        }
        assert!(
            p.config_fields[1..].iter().all(|field| !field.required
                && field.default_value.is_none()
                && field.role.is_none())
        );
        assert_eq!(p.config_fields[4].field_type, ConfigFieldType::Password);
    }
    #[test]
    fn connection_probe_checks_service_dependencies() {
        assert!(validate_service_status(br#"{"yt_dlp":"2026.10.26","error":null}"#).is_ok());
        assert!(validate_service_status(br#"{"yt_dlp":null,"error":"not installed"}"#).is_err());
        assert!(validate_service_status(br#"{}"#).is_err());
    }
    #[test]
    fn episode_and_id_requests_are_searches_even_without_text() {
        for fields in [
            serde_json::json!({"query":"", "limit":20, "season":1,"episode":2}),
            serde_json::json!({"query":"", "limit":20, "ids":{"tvdb":"388373"}}),
        ] {
            let request = serde_json::from_value(fields).unwrap();
            assert!(!is_connection_probe(&request));
        }
        let probe = serde_json::from_value(serde_json::json!({"query":"", "limit":20})).unwrap();
        assert!(is_connection_probe(&probe));
    }
}
