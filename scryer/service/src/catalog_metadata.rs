//! Read-only episode lookup against the operator's Scryer catalog.
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use reqwest::{
    Client, Response, Url,
    header::{COOKIE, SET_COOKIE},
};
use scryer_plugin_sdk::PluginSearchRequest;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const TITLE_FIELDS: &str =
    "id name externalIds { source value } collections { id collectionIndex }";

#[derive(Debug)]
struct AuthenticationRejected;

impl std::fmt::Display for AuthenticationRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Scryer metadata authentication rejected")
    }
}

impl std::error::Error for AuthenticationRejected {}

// Deliberately no Debug: this DTO contains request-scoped credentials.
#[derive(Clone, Deserialize, Serialize)]
pub struct MetadataConnection {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResolvedEpisode {
    pub title_id: String,
    pub series_name: String,
    pub external_ids: HashMap<String, String>,
    pub season: u32,
    pub episode: u32,
    pub episode_title: String,
    pub air_date: Option<String>,
    pub duration_seconds: Option<u64>,
}

#[derive(Clone)]
enum Authorization {
    Bearer(String),
    Authless { proof: String, cookie: String },
}

struct CachedAuthorization {
    key: [u8; 32],
    authorization: Authorization,
    expires_at: i64,
}

pub struct MetadataClient {
    http: Client,
    cache: Mutex<VecDeque<CachedAuthorization>>,
}

impl MetadataClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            // A redirect must never forward a proof or credential to another endpoint.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| anyhow!("could not initialize metadata HTTP client"))?;
        Ok(Self {
            http,
            cache: Mutex::new(VecDeque::new()),
        })
    }

    pub async fn resolve(
        &self,
        connection: &MetadataConnection,
        title_id: Option<&str>,
        request: &PluginSearchRequest,
    ) -> Result<ResolvedEpisode> {
        let base = endpoint(&connection.url)?;
        let key = authentication_key(&base, connection);
        // A server restart or revoked session can invalidate a token/proof before its expiry.
        // Retry the read-only lookup once, with freshly acquired authentication.
        for attempt in 0..=1 {
            match self.resolve_attempt(connection, title_id, request).await {
                Err(error) if error.is::<AuthenticationRejected>() => {
                    self.evict_authentication(&key)?;
                    if attempt == 1 {
                        return Err(error);
                    }
                }
                result => return result,
            }
        }
        unreachable!("authentication retry loop always returns after its second attempt")
    }

    fn evict_authentication(&self, key: &[u8; 32]) -> Result<()> {
        self.cache
            .lock()
            .map_err(|_| anyhow!("metadata authentication cache unavailable"))?
            .retain(|entry| &entry.key != key);
        Ok(())
    }

    async fn resolve_attempt(
        &self,
        connection: &MetadataConnection,
        title_id: Option<&str>,
        request: &PluginSearchRequest,
    ) -> Result<ResolvedEpisode> {
        let season = request
            .season
            .ok_or_else(|| anyhow!("episode metadata requires a season number"))?;
        let episode = request
            .episode
            .ok_or_else(|| anyhow!("episode metadata requires an episode number"))?;
        let base = endpoint(&connection.url)?;
        let authorization = self.authorize(&base, connection).await?;
        let title = if let Some(id) = title_id {
            ensure!(!id.trim().is_empty(), "configured Scryer title ID is empty");
            let data = self
                .query(
                    &base,
                    &authorization,
                    &format!("query($id: ID!) {{ title(id: $id) {{ {TITLE_FIELDS} }} }}"),
                    json!({"id":id}),
                )
                .await?;
            parse_title(data.get("title").cloned().unwrap_or(Value::Null))?
        } else {
            self.find_title(&base, &authorization, request).await?
        };
        ensure!(
            !title.id.trim().is_empty() && !title.name.trim().is_empty(),
            "Scryer title metadata is incomplete"
        );
        ensure!(
            title.collections.len() <= 1000,
            "Scryer returned too many collections"
        );
        let mut collections = title
            .collections
            .iter()
            .filter(|c| c.collection_index.trim().parse::<u32>().ok() == Some(season));
        let collection = collections
            .next()
            .ok_or_else(|| anyhow!("requested season is missing from Scryer metadata"))?;
        ensure!(
            collections.next().is_none(),
            "requested season is ambiguous in Scryer metadata"
        );
        let data = self.query(&base, &authorization,
            "query($id: ID!) { collectionById(id: $id) { episodes { seasonNumber episodeNumber title airDate durationSeconds } } }",
            json!({"id":collection.id})).await?;
        let episodes: Vec<Episode> = serde_json::from_value(
            data.pointer("/collectionById/episodes")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .map_err(|_| anyhow!("Scryer season metadata is missing or invalid"))?;
        ensure!(
            episodes.len() <= 10000,
            "Scryer returned too many season episodes"
        );
        let mut matches = episodes.into_iter().filter(|e| {
            e.season_number
                .as_deref()
                .and_then(|n| n.parse::<u32>().ok())
                == Some(season)
                && e.episode_number
                    .as_deref()
                    .and_then(|n| n.parse::<u32>().ok())
                    == Some(episode)
        });
        let matched = matches
            .next()
            .ok_or_else(|| anyhow!("requested episode is missing from Scryer metadata"))?;
        ensure!(
            matches.next().is_none(),
            "requested episode is ambiguous in Scryer metadata"
        );
        let episode_title = matched
            .title
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Scryer episode title is missing"))?;
        if let Some(date) = &matched.air_date {
            chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .map_err(|_| anyhow!("Scryer episode air date is invalid"))?;
        }
        let mut external_ids = HashMap::new();
        for id in title.external_ids {
            if let Some(source) = external_source(&id.source) {
                if let Some(previous) = external_ids.insert(source.to_owned(), id.value.clone()) {
                    ensure!(
                        previous == id.value,
                        "Scryer title has conflicting external IDs"
                    );
                }
            }
        }
        Ok(ResolvedEpisode {
            title_id: title.id,
            series_name: title.name,
            external_ids,
            season,
            episode,
            episode_title,
            air_date: matched.air_date,
            duration_seconds: matched
                .duration_seconds
                .and_then(|n| u64::try_from(n).ok())
                .filter(|n| *n > 0),
        })
    }

    async fn find_title(
        &self,
        base: &Url,
        auth: &Authorization,
        request: &PluginSearchRequest,
    ) -> Result<Title> {
        let mut found: HashMap<String, Title> = HashMap::new();
        let mut ids: Vec<_> = request
            .ids
            .iter()
            .filter_map(|(k, v)| external_source(k).map(|s| (s, v)))
            .collect();
        ids.sort();
        for (source, value) in &ids {
            ensure!(
                !value.trim().is_empty() && value.len() <= 256,
                "external title ID is invalid"
            );
            let data = self.query(base, auth,
                &format!("query($source: String!, $values: [String!]!) {{ titlesByExternalIds(source: $source, values: $values) {{ {TITLE_FIELDS} }} }}"),
                json!({"source":source,"values":[value]})).await?;
            let titles: Vec<Title> = serde_json::from_value(
                data.get("titlesByExternalIds")
                    .cloned()
                    .unwrap_or(Value::Null),
            )
            .map_err(|_| anyhow!("Scryer external-ID lookup response is invalid"))?;
            ensure!(titles.len() <= 10, "Scryer external-ID lookup is ambiguous");
            for title in titles {
                found.insert(title.id.clone(), title);
            }
        }
        ensure!(
            found.len() <= 1,
            "external IDs identify multiple Scryer titles"
        );
        if let Some(title) = found.into_values().next() {
            // Do not silently prefer one supplied identity over a conflicting one.
            ensure!(
                ids.iter().all(|(source, value)| title
                    .external_ids
                    .iter()
                    .any(|id| external_source(&id.source) == Some(*source) && id.value == **value)),
                "Scryer title does not match all supplied external IDs"
            );
            return Ok(title);
        }
        // A known external ID must not be replaced by an unrelated textual match.
        ensure!(
            ids.is_empty(),
            "external title IDs were not found in Scryer"
        );
        let query = series_query(&request.query);
        ensure!(
            !query.is_empty() && query.len() <= 512,
            "series search text is missing or too long"
        );
        let data = self.query(base, auth,
            &format!("query($query: String!) {{ titles(query: $query, limit: 10) {{ hasMore items {{ {TITLE_FIELDS} }} }} }}"), json!({"query":query})).await?;
        ensure!(
            data.pointer("/titles/hasMore").and_then(Value::as_bool) == Some(false),
            "series lookup is incomplete; configure a Scryer title ID"
        );
        let titles: Vec<Title> = serde_json::from_value(
            data.pointer("/titles/items")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .map_err(|_| anyhow!("Scryer title search response is invalid"))?;
        ensure!(
            titles.len() <= 10,
            "Scryer returned too many title search results"
        );
        let mut matches = titles
            .into_iter()
            .filter(|t| normalized(&t.name) == normalized(&query));
        let title = matches
            .next()
            .ok_or_else(|| anyhow!("exact series title was not found in Scryer"))?;
        ensure!(
            matches.next().is_none(),
            "series title is ambiguous; configure a Scryer title ID"
        );
        Ok(title)
    }

    async fn authorize(
        &self,
        base: &Url,
        connection: &MetadataConnection,
    ) -> Result<Authorization> {
        let key = authentication_key(base, connection);
        let now = Utc::now().timestamp();
        {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow!("metadata authentication cache unavailable"))?;
            cache.retain(|entry| entry.expires_at > now + 15);
            if let Some(entry) = cache.iter().find(|e| e.key == key) {
                return Ok(entry.authorization.clone());
            }
        }
        let (authorization, expires_at) = match (&connection.username, &connection.password) {
            (Some(username), Some(password))
                if !username.trim().is_empty() && !password.is_empty() =>
            {
                let data = self.raw_query(base, None,
                    "mutation($input: LoginInput!) { login(input: $input) { token expiresAt } }",
                    json!({"input":{"username":username,"password":password,"persistSession":false}})).await?;
                let token = data
                    .pointer("/login/token")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("Scryer login did not return a token"))?;
                let expires = data
                    .pointer("/login/expiresAt")
                    .and_then(Value::as_str)
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .ok_or_else(|| anyhow!("Scryer login did not return a valid expiry"))?
                    .timestamp();
                (Authorization::Bearer(token.to_owned()), expires)
            }
            (None, None) => {
                let response = self
                    .http
                    .get(
                        base.join("authless-client")
                            .map_err(|_| anyhow!("invalid metadata endpoint"))?,
                    )
                    .send()
                    .await
                    .map_err(|_| anyhow!("Scryer authless connection failed"))?;
                let cookie = response
                    .headers()
                    .get_all(SET_COOKIE)
                    .iter()
                    .filter_map(|h| h.to_str().ok())
                    .filter_map(|s| s.split(';').next())
                    .collect::<Vec<_>>()
                    .join("; ");
                let body = bounded_json(response).await?;
                let proof = body
                    .get("proof")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("Scryer authless proof is unavailable"))?;
                let expires = body
                    .get("expiresAt")
                    .or_else(|| body.get("expires_at"))
                    .and_then(Value::as_i64)
                    .ok_or_else(|| anyhow!("Scryer authless proof expiry is invalid"))?;
                ensure!(!cookie.is_empty(), "Scryer authless cookie is unavailable");
                (
                    Authorization::Authless {
                        proof: proof.to_owned(),
                        cookie,
                    },
                    expires,
                )
            }
            _ => bail!("provide both Scryer username and password, or neither"),
        };
        ensure!(expires_at > now + 15, "Scryer authentication is expired");
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("metadata authentication cache unavailable"))?;
        cache.retain(|entry| entry.key != key && entry.expires_at > now + 15);
        while cache.len() >= 8 {
            cache.pop_front();
        }
        cache.push_back(CachedAuthorization {
            key,
            authorization: authorization.clone(),
            expires_at,
        });
        Ok(authorization)
    }

    async fn query(
        &self,
        base: &Url,
        auth: &Authorization,
        query: &str,
        variables: Value,
    ) -> Result<Value> {
        self.raw_query(base, Some(auth), query, variables).await
    }

    async fn raw_query(
        &self,
        base: &Url,
        auth: Option<&Authorization>,
        query: &str,
        variables: Value,
    ) -> Result<Value> {
        let mut request = self
            .http
            .post(
                base.join("graphql")
                    .map_err(|_| anyhow!("invalid metadata endpoint"))?,
            )
            .json(&json!({"query":query,"variables":variables}));
        request = match auth {
            Some(Authorization::Bearer(token)) => request.bearer_auth(token),
            Some(Authorization::Authless { proof, cookie }) => request
                .header(COOKIE, cookie)
                .header("x-scryer-web-client", proof),
            None => request,
        };
        let response = request
            .send()
            .await
            .map_err(|_| anyhow!("Scryer metadata request failed"))?;
        let body = bounded_json(response).await?;
        if let Some(errors) = body
            .get("errors")
            .and_then(Value::as_array)
            .filter(|e| !e.is_empty())
        {
            if errors.iter().any(|error| {
                error
                    .pointer("/extensions/code")
                    .and_then(Value::as_str)
                    .is_some_and(|code| {
                        matches!(
                            code,
                            "UNAUTHENTICATED"
                                | "UNAUTHORIZED"
                                | "FORBIDDEN"
                                | "AUTH_REQUIRED"
                                | "AUTHENTICATION_REQUIRED"
                        )
                    })
            }) {
                return Err(AuthenticationRejected.into());
            }
            // Never return arbitrary server messages: they may echo request input.
            let code = errors[0]
                .pointer("/extensions/code")
                .and_then(Value::as_str)
                .unwrap_or("GRAPHQL_ERROR");
            let safe_code = match code {
                "UNAUTHENTICATED"
                | "UNAUTHORIZED"
                | "FORBIDDEN"
                | "AUTH_REQUIRED"
                | "AUTHENTICATION_REQUIRED" => "AUTHENTICATION_REQUIRED",
                "NOT_FOUND" => "NOT_FOUND",
                "RATE_LIMITED" | "RATE_LIMIT_EXCEEDED" => "RATE_LIMITED",
                _ => "GRAPHQL_ERROR",
            };
            bail!("Scryer metadata GraphQL error: {safe_code}");
        }
        body.get("data")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or_else(|| anyhow!("Scryer metadata response has no data"))
    }
}

fn authentication_key(base: &Url, connection: &MetadataConnection) -> [u8; 32] {
    let mut digest = Sha256::new();
    for component in [
        base.as_str(),
        connection.username.as_deref().unwrap_or(""),
        connection.password.as_deref().unwrap_or(""),
    ] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component.as_bytes());
    }
    digest.finalize().into()
}

fn endpoint(input: &str) -> Result<Url> {
    let mut url = Url::parse(input).map_err(|_| anyhow!("invalid Scryer metadata URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Scryer metadata URL must use HTTP or HTTPS"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Scryer metadata URL must not contain credentials, query, or fragment"
    );
    let path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

async fn bounded_json(mut response: Response) -> Result<Value> {
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(AuthenticationRejected.into());
    }
    ensure!(
        response.status().is_success(),
        "Scryer metadata HTTP request rejected ({})",
        response.status().as_u16()
    );
    ensure!(
        response.content_length().unwrap_or(0) <= MAX_RESPONSE as u64,
        "Scryer metadata response exceeds size limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("Scryer metadata response could not be read"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_RESPONSE,
            "Scryer metadata response exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("Scryer metadata response is invalid JSON"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Title {
    id: String,
    name: String,
    external_ids: Vec<ExternalId>,
    collections: Vec<Collection>,
}
#[derive(Deserialize)]
struct ExternalId {
    source: String,
    value: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Collection {
    id: String,
    collection_index: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Episode {
    season_number: Option<String>,
    episode_number: Option<String>,
    title: Option<String>,
    air_date: Option<String>,
    duration_seconds: Option<i64>,
}

fn parse_title(value: Value) -> Result<Title> {
    serde_json::from_value(value)
        .map_err(|_| anyhow!("Scryer title metadata is missing or invalid"))
}

fn external_source(key: &str) -> Option<&'static str> {
    match key.trim().to_ascii_lowercase().as_str() {
        "tvdb" | "tvdb_id" | "tvdbid" => Some("tvdb"),
        "tmdb" | "tmdb_id" | "tmdbid" => Some("tmdb"),
        "imdb" | "imdb_id" | "imdbid" => Some("imdb"),
        _ => None,
    }
}

fn normalized(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn series_query(query: &str) -> String {
    // Only remove an explicit S<digits>E<digits> suffix token; never guess numbering.
    let lower = query.to_ascii_lowercase();
    for (index, _) in lower.match_indices('s') {
        if index > 0 && lower.as_bytes()[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let tail = &lower.as_bytes()[index + 1..];
        let digits = tail.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 || tail.get(digits) != Some(&b'e') {
            continue;
        }
        let ep_digits = tail[digits + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if ep_digits > 0 {
            return query[..index]
                .trim_end_matches(|c: char| c.is_whitespace() || c == '.' || c == '-' || c == '_')
                .to_owned();
        }
    }
    query.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::HeaderMap,
        routing::{get, post},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone)]
    struct MockState {
        logins: Arc<AtomicUsize>,
        duplicate: bool,
        authless: bool,
    }

    async fn graphql(
        State(state): State<MockState>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let query = body["query"].as_str().unwrap();
        if query.starts_with("mutation") {
            assert_eq!(body["variables"]["input"]["persistSession"], false);
            state.logins.fetch_add(1, Ordering::SeqCst);
            return Json(
                json!({"data":{"login":{"token":"fixture-token","expiresAt":(Utc::now()+chrono::Duration::minutes(10)).to_rfc3339()}}}),
            );
        }
        if state.authless {
            assert_eq!(headers["cookie"], "fixture=fixture-cookie");
            assert_eq!(headers["x-scryer-web-client"], "fixture-proof");
        } else {
            assert_eq!(headers["authorization"], "Bearer fixture-token");
        }
        if query.contains("collectionById") {
            let episode = json!({"seasonNumber":"2","episodeNumber":"3","title":"The Actual Episode","airDate":"2026-11-20","durationSeconds":1234});
            let episodes = if state.duplicate {
                vec![episode.clone(), episode]
            } else {
                vec![episode]
            };
            Json(json!({"data":{"collectionById":{"episodes":episodes}}}))
        } else {
            assert!(
                !query.contains("episodes"),
                "title lookup must not fetch the entire series"
            );
            Json(
                json!({"data":{"title":{"id":"title-1","name":"Fixture Series","externalIds":[{"source":"tvdb","value":"123"}],"collections":[{"id":"season-1","collectionIndex":"1"},{"id":"season-2","collectionIndex":"2"}]}}}),
            )
        }
    }

    async fn mock(
        duplicate: bool,
        authless: bool,
    ) -> (
        MetadataConnection,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let logins = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/graphql", post(graphql))
            .route(
                "/authless-client",
                get(|| async {
                    (
                        [("set-cookie", "fixture=fixture-cookie; HttpOnly; Path=/")],
                        Json(
                            json!({"proof":"fixture-proof","expiresAt":Utc::now().timestamp()+600}),
                        ),
                    )
                }),
            )
            .with_state(MockState {
                logins: logins.clone(),
                duplicate,
                authless,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            MetadataConnection {
                url,
                username: (!authless).then(|| "fixture-user".into()),
                password: (!authless).then(|| "fixture-password".into()),
            },
            logins,
            server,
        )
    }

    fn request() -> PluginSearchRequest {
        PluginSearchRequest {
            query: "Fixture Series S02E03".into(),
            season: Some(2),
            episode: Some(3),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn resolves_exact_episode_and_reuses_memory_only_login() {
        let (connection, logins, server) = mock(false, false).await;
        let client = MetadataClient::new().unwrap();
        for _ in 0..2 {
            let result = client
                .resolve(&connection, Some("title-1"), &request())
                .await
                .unwrap();
            assert_eq!((result.season, result.episode), (2, 3));
            assert_eq!(result.episode_title, "The Actual Episode");
            assert_eq!(result.air_date.as_deref(), Some("2026-11-20"));
            assert_eq!(result.duration_seconds, Some(1234));
            assert_eq!(result.external_ids.get("tvdb").unwrap(), "123");
        }
        assert_eq!(logins.load(Ordering::SeqCst), 1);
        assert_eq!(client.cache.lock().unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn supports_authless_cookie_and_rejects_duplicate_episode() {
        let (connection, logins, server) = mock(true, true).await;
        let error = MetadataClient::new()
            .unwrap()
            .resolve(&connection, Some("title-1"), &request())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
        assert_eq!(logins.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn refreshes_rejected_cached_token_once_and_bounds_invalid_login_retries() {
        use axum::{http::StatusCode, response::IntoResponse};
        #[derive(Clone)]
        struct RetryState {
            logins: Arc<AtomicUsize>,
            rejections: Arc<AtomicUsize>,
            invalid_login: bool,
            http_rejection: bool,
        }
        async fn handler(
            State(state): State<RetryState>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            let login = body["query"].as_str().unwrap().starts_with("mutation");
            if login {
                state.logins.fetch_add(1, Ordering::SeqCst);
            }
            if (login && state.invalid_login)
                || (!login
                    && headers
                        .get("authorization")
                        .is_some_and(|value| value == "Bearer stale-fixture-token"))
            {
                state.rejections.fetch_add(1, Ordering::SeqCst);
                if state.http_rejection {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                return Json(json!({"errors":[{"message":"fixture-password must never escape","extensions":{"code":"AUTHENTICATION_REQUIRED"}}]})).into_response();
            }
            if login {
                return Json(json!({"data":{"login":{"token":"fixture-token","expiresAt":(Utc::now()+chrono::Duration::minutes(10)).to_rfc3339()}}})).into_response();
            }
            graphql(
                State(MockState {
                    logins: state.logins,
                    duplicate: false,
                    authless: false,
                }),
                headers,
                Json(body),
            )
            .await
            .into_response()
        }
        for http_rejection in [false, true] {
            for invalid_login in [false, true] {
                let state = RetryState {
                    logins: Arc::new(AtomicUsize::new(0)),
                    rejections: Arc::new(AtomicUsize::new(0)),
                    invalid_login,
                    http_rejection,
                };
                let app = Router::new()
                    .route("/graphql", post(handler))
                    .with_state(state.clone());
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let connection = MetadataConnection {
                    url: format!("http://{}", listener.local_addr().unwrap()),
                    username: Some("fixture-user".into()),
                    password: Some("fixture-password".into()),
                };
                let server = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let client = MetadataClient::new().unwrap();
                if !invalid_login {
                    client.cache.lock().unwrap().push_back(CachedAuthorization {
                        key: authentication_key(&endpoint(&connection.url).unwrap(), &connection),
                        authorization: Authorization::Bearer("stale-fixture-token".into()),
                        expires_at: Utc::now().timestamp() + 600,
                    });
                }
                let result = client
                    .resolve(&connection, Some("title-1"), &request())
                    .await;
                if invalid_login {
                    let error = result.unwrap_err();
                    assert!(error.is::<AuthenticationRejected>());
                    assert!(!error.to_string().contains("fixture-password"));
                    assert_eq!(state.logins.load(Ordering::SeqCst), 2);
                    assert_eq!(state.rejections.load(Ordering::SeqCst), 2);
                    assert!(client.cache.lock().unwrap().is_empty());
                } else {
                    assert_eq!(result.unwrap().episode_title, "The Actual Episode");
                    assert_eq!(state.logins.load(Ordering::SeqCst), 1);
                    assert_eq!(state.rejections.load(Ordering::SeqCst), 1);
                    assert_eq!(client.cache.lock().unwrap().len(), 1);
                }
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn refuses_missing_numbers_before_network() {
        let connection = MetadataConnection {
            url: "http://127.0.0.1:1".into(),
            username: None,
            password: None,
        };
        let error = MetadataClient::new()
            .unwrap()
            .resolve(&connection, None, &PluginSearchRequest::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("season number"));
    }

    #[test]
    fn validates_urls_without_echoing_secrets_and_strips_only_episode_token() {
        for input in [
            "https://user:secret@example.com",
            "https://example.com?token=secret",
            "file:///secret",
        ] {
            let error = endpoint(input).unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
        assert_eq!(series_query("Fixture.Series.S02E03"), "Fixture.Series");
        assert_eq!(series_query("Series Seven"), "Series Seven");
        assert_eq!(normalized("Fixture.Series"), normalized("Fixture Series"));
    }
}
