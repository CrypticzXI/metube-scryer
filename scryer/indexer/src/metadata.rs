use crate::sources::SourceMapping;
use scryer_plugin_sdk::PluginSearchRequest;
use serde::Serialize;

/// Credentials travel only to the configured indexer service; this plugin never
/// contacts Scryer or includes credentials in error/debug output.
#[derive(Serialize)]
pub(crate) struct MetadataConnection {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct SearchEnvelope<'a> {
    pub request: &'a PluginSearchRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_connection: Option<MetadataConnection>,
    pub sources: Vec<SourceMapping>,
}

pub(crate) fn connection(
    url: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<Option<MetadataConnection>, &'static str> {
    let url = url.map(str::trim).filter(|value| !value.is_empty());
    let username = username.map(str::trim).filter(|value| !value.is_empty());
    // Password whitespace is significant, but an empty value means open-admin.
    let password = password.filter(|value| !value.is_empty());
    if url.is_none() && password.is_none() {
        return Ok(None);
    }
    let url = url.unwrap_or("http://127.0.0.1:8080");
    let parsed = url::Url::parse(url)
        .map_err(|_| "Scryer URL must be an HTTP(S) address without credentials")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("Scryer URL must be an HTTP(S) address without credentials, query or fragment");
    }
    Ok(Some(MetadataConnection {
        url: url.trim_end_matches('/').to_owned(),
        username: password.map(|_| username.unwrap_or("admin").to_owned()),
        password: password.map(str::to_owned),
    }))
}

pub(crate) fn search_body(
    request: &PluginSearchRequest,
    metadata_connection: Option<MetadataConnection>,
    sources: Vec<SourceMapping>,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&SearchEnvelope {
        request,
        metadata_connection,
        sources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_connection_preserves_base_url_only_configuration() {
        assert!(connection(None, None, None).unwrap().is_none());
        assert!(connection(None, Some("admin"), Some("")).unwrap().is_none());
        let request: PluginSearchRequest =
            serde_json::from_value(serde_json::json!({"query":"Sidemen Sundays", "limit":20}))
                .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&search_body(&request, None, vec![]).unwrap()).unwrap();
        assert_eq!(body["request"]["query"], "Sidemen Sundays");
        assert!(body.get("metadata_connection").is_none());
        assert_eq!(body["sources"], serde_json::json!([]));
    }

    #[test]
    fn explicit_url_supports_open_admin_without_credentials() {
        let connection = connection(Some("http://scryer:8080/"), Some("admin"), None)
            .unwrap()
            .unwrap();
        let json = serde_json::to_value(connection).unwrap();
        assert_eq!(json, serde_json::json!({"url":"http://scryer:8080"}));
    }

    #[test]
    fn forwards_explicit_sources_without_losing_structured_request() {
        let sources = crate::sources::parse(Some(r#"[{"series_name":"Sidemen Sundays","tvdb_id":"388373","channel_url":"https://www.youtube.com/@Sidemen"}]"#)).unwrap();
        let request = PluginSearchRequest {
            query: "Sidemen Sundays".into(),
            season: Some(2026),
            episode: Some(47),
            ..Default::default()
        };
        let body: serde_json::Value =
            serde_json::from_slice(&search_body(&request, None, sources).unwrap()).unwrap();
        assert_eq!(body["request"]["episode"], 47);
        assert_eq!(body["sources"][0]["tvdb_id"], "388373");
        assert_eq!(
            body["sources"][0]["channel_url"],
            "https://www.youtube.com/@Sidemen"
        );
        assert!(body.get("metadata_connection").is_none());
    }

    #[test]
    fn password_selects_authenticated_connection_and_preserves_spaces() {
        let connection = connection(None, None, Some(" secret ")).unwrap().unwrap();
        let request: PluginSearchRequest = serde_json::from_value(
            serde_json::json!({"query":"Sidemen Sundays", "season":2,"episode":4,"limit":20}),
        )
        .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&search_body(&request, Some(connection), vec![]).unwrap())
                .unwrap();
        assert_eq!(body["request"]["season"], 2);
        assert_eq!(body["request"]["episode"], 4);
        assert_eq!(
            body["metadata_connection"],
            serde_json::json!({"url":"http://127.0.0.1:8080","username":"admin","password":" secret "})
        );
    }

    #[test]
    fn rejects_embedded_credentials_and_non_http_urls() {
        for url in [
            "file:///tmp/scryer",
            "http://user:secret@scryer",
            "http://scryer?token=secret",
            "not a url",
        ] {
            let error = connection(Some(url), None, None).err().unwrap();
            assert!(!error.contains("secret"));
        }
    }
}
