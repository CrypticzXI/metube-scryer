use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceMapping {
    pub series_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scryer_title_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tvdb_id: Option<String>,
    pub channel_url: String,
}

pub(crate) fn parse(raw: Option<&str>) -> Result<Vec<SourceMapping>, &'static str> {
    if raw.is_some_and(|raw| raw.len() > 64 * 1024) {
        return Err("Official series sources JSON exceeds 64 KiB");
    }
    let raw = raw.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("[]");
    if raw.len() > 64 * 1024 {
        return Err("Official series sources JSON exceeds 64 KiB");
    }
    let mut mappings: Vec<SourceMapping> = serde_json::from_str(raw)
        .map_err(|_| "Official series sources must be a JSON array of series_name, optional scryer_title_id/tvdb_id, and channel_url")?;
    if mappings.len() > 100 {
        return Err("Official series sources supports at most 100 sources");
    }
    for mapping in &mut mappings {
        mapping.series_name = mapping.series_name.trim().to_owned();
        if mapping.series_name.is_empty() || mapping.series_name.len() > 200 {
            return Err("Each official source needs a series name between 1 and 200 bytes");
        }
        if let Some(id) = &mapping.tvdb_id {
            if id.is_empty()
                || id.len() > 20
                || !id.bytes().all(|c| c.is_ascii_digit())
                || id.bytes().all(|c| c == b'0')
            {
                return Err("Official source TVDB IDs must be positive numeric strings");
            }
        }
        if let Some(id) = &mapping.scryer_title_id {
            if !is_uuid(id) {
                return Err("Official source Scryer title IDs must be UUIDs");
            }
        }
        mapping.channel_url = mapping.channel_url.trim().to_owned();
        if !is_collection_url(&mapping.channel_url) {
            return Err(
                "Official source channel_url must be an HTTPS YouTube channel or playlist URL without credentials",
            );
        }
    }
    Ok(mappings)
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn is_collection_url(value: &str) -> bool {
    if value.len() > 2048 {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || !matches!(
            url.host_str(),
            Some("youtube.com" | "www.youtube.com" | "m.youtube.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let mut parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    if parts == ["playlist"] {
        let params: Vec<_> = url.query_pairs().collect();
        return params.len() == 1
            && params[0].0 == "list"
            && !params[0].1.is_empty()
            && params[0].1.len() <= 200
            && params[0]
                .1
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c));
    }
    if url.query().is_some() {
        return false;
    }
    if parts.len() > 1 && matches!(parts.last(), Some(&"videos" | &"streams")) {
        parts.pop();
    }
    match parts.as_slice() {
        [handle] => handle.starts_with('@') && handle.len() > 1 && !handle.contains('%'),
        ["channel", id] => {
            id.starts_with("UC")
                && id.len() == 24
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        }
        ["c" | "user", name] => !name.is_empty() && !name.contains('%'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_config_and_explicit_official_sources() {
        assert!(parse(None).unwrap().is_empty());
        let parsed=parse(Some(r#"[{"series_name":" Sidemen Sundays ","tvdb_id":"388373","scryer_title_id":"00000000-0000-4000-8000-000000000001","channel_url":"https://www.youtube.com/@Sidemen"}]"#)).unwrap();
        assert_eq!(parsed[0].series_name, "Sidemen Sundays");
        assert_eq!(parsed[0].tvdb_id.as_deref(), Some("388373"));
        assert!(is_collection_url(
            "https://www.youtube.com/playlist?list=PL_example"
        ));
        assert!(is_collection_url("https://www.youtube.com/@Sidemen/videos"));
        assert!(is_collection_url(
            "https://www.youtube.com/channel/UCDogdKl7t7NHzQ95aEwkdMw/streams"
        ));
    }
    #[test]
    fn rejects_untrusted_urls_and_bad_identifiers_without_echoing_input() {
        for channel_url in [
            "https://evil.example/@Sidemen",
            "http://youtube.com/@Sidemen",
            "https://user:secret@youtube.com/@Sidemen",
            "https://www.youtube.com/watch?v=abcdefghijk",
            "https://youtu.be/abcdefghijk",
            "https://www.youtube.com/playlist?list=PL_x&token=secret",
        ] {
            let raw =
                serde_json::json!([{"series_name":"Series","channel_url":channel_url}]).to_string();
            assert!(!parse(Some(&raw)).unwrap_err().contains("secret"));
        }
        for extra in [
            serde_json::json!({"tvdb_id":"oops"}),
            serde_json::json!({"scryer_title_id":"not-a-uuid"}),
        ] {
            let mut mapping = serde_json::json!({"series_name":"Series","channel_url":"https://www.youtube.com/@Sidemen"});
            mapping
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(parse(Some(&serde_json::json!([mapping]).to_string())).is_err());
        }
    }
    #[test]
    fn bounds_source_count_and_payload() {
        let mapping = serde_json::json!({"series_name":"Series","channel_url":"https://www.youtube.com/@Sidemen"});
        assert!(parse(Some(&serde_json::to_string(&vec![mapping; 101]).unwrap())).is_err());
        assert!(parse(Some(&" ".repeat(65537))).is_err());
        assert!(parse(Some(&format!("[{}]", "x".repeat(65537)))).is_err());
        assert!(
            parse(Some(
                r#"[{"series_name":"","channel_url":"https://youtube.com/@x"}]"#
            ))
            .is_err()
        );
    }
}
