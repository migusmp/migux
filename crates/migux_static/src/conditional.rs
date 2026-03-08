#[derive(Debug)]
enum IfNoneMatch {
    Any,
    Tags(Vec<String>),
}

/// Returns true if the request is GET/HEAD with If-None-Match matching the current ETag (→ 304).
pub(crate) fn should_return_not_modified(method: &str, headers: &str, etag_value: &str) -> bool {
    if method != "GET" && method != "HEAD" {
        return false;
    }
    let Some(if_none_match) = parse_if_none_match(headers) else {
        return false;
    };
    if_none_match_satisfied(&if_none_match, etag_value)
}

fn parse_if_none_match(headers: &str) -> Option<IfNoneMatch> {
    let mut combined = String::new();
    for line in headers.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("if-none-match") {
            if !combined.is_empty() {
                combined.push(',');
            }
            combined.push_str(value.trim());
        }
    }

    if combined.is_empty() {
        None
    } else {
        parse_if_none_match_value(&combined)
    }
}

fn parse_if_none_match_value(value: &str) -> Option<IfNoneMatch> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value == "*" {
        return Some(IfNoneMatch::Any);
    }

    let mut tags = Vec::new();
    for token in value.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if token == "*" {
            return Some(IfNoneMatch::Any);
        }
        if let Some(tag) = normalize_etag_token(token) {
            tags.push(tag);
        }
    }

    if tags.is_empty() {
        None
    } else {
        Some(IfNoneMatch::Tags(tags))
    }
}

fn normalize_etag_token(token: &str) -> Option<String> {
    let mut tag = token.trim();
    if tag.is_empty() {
        return None;
    }

    if let Some(stripped) = tag.strip_prefix("W/").or_else(|| tag.strip_prefix("w/")) {
        tag = stripped.trim();
    }

    if (tag.starts_with('"') && tag.ends_with('"'))
        || (tag.starts_with('\'') && tag.ends_with('\''))
    {
        tag = &tag[1..tag.len() - 1];
    }

    let tag = tag.trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

fn if_none_match_satisfied(if_none_match: &IfNoneMatch, etag_value: &str) -> bool {
    match if_none_match {
        IfNoneMatch::Any => true,
        IfNoneMatch::Tags(tags) => tags.iter().any(|tag| tag == etag_value),
    }
}

/// Returns true if the request is GET/HEAD with If-Modified-Since and the file was not
/// modified since that date (→ 304). Only considered when If-None-Match is not used (RFC 7232).
/// File mtime is compared at 1-second granularity (HTTP-date has no subseconds).
pub(crate) fn should_return_not_modified_if_modified_since(
    method: &str,
    headers: &str,
    file_mtime: Option<std::time::SystemTime>,
) -> bool {
    if method != "GET" && method != "HEAD" {
        return false;
    }
    let Some(client_date) = parse_if_modified_since(headers) else {
        return false;
    };
    let Some(mtime) = file_mtime else {
        return false;
    };
    // HTTP-date has second precision; truncate file mtime to seconds so comparison matches
    // what we send in Last-Modified (e.g. client echoes "Fri, 08 Mar 2025 12:34:56 GMT").
    let mtime_secs = truncate_to_seconds(mtime);
    mtime_secs <= client_date
}

fn truncate_to_seconds(t: std::time::SystemTime) -> std::time::SystemTime {
    use std::time::UNIX_EPOCH;
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    UNIX_EPOCH + std::time::Duration::from_secs(secs)
}

fn parse_if_modified_since(headers: &str) -> Option<std::time::SystemTime> {
    for line in headers.lines().skip(1) {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("if-modified-since") {
            let value = value.trim().trim_end_matches('\r');
            if value.is_empty() {
                return None;
            }
            return httpdate::parse_http_date(value).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        IfNoneMatch, if_none_match_satisfied, parse_if_modified_since, parse_if_none_match,
        parse_if_none_match_value, should_return_not_modified_if_modified_since,
    };

    #[test]
    fn parse_if_none_match_single() {
        let parsed = parse_if_none_match_value(r#""abc""#).expect("expected tags");
        assert!(if_none_match_satisfied(&parsed, "abc"));
        assert!(!if_none_match_satisfied(&parsed, "nope"));
    }

    #[test]
    fn parse_if_none_match_list() {
        let parsed = parse_if_none_match_value(r#""a", "b", W/"c""#).expect("expected tags");
        assert!(if_none_match_satisfied(&parsed, "b"));
        assert!(if_none_match_satisfied(&parsed, "c"));
        assert!(!if_none_match_satisfied(&parsed, "d"));
        match parsed {
            IfNoneMatch::Tags(tags) => assert_eq!(tags, vec!["a", "b", "c"]),
            IfNoneMatch::Any => panic!("expected tag list"),
        }
    }

    #[test]
    fn parse_if_none_match_wildcard() {
        let parsed = parse_if_none_match_value("*").expect("expected wildcard");
        assert!(matches!(parsed, IfNoneMatch::Any));
        assert!(if_none_match_satisfied(&parsed, "anything"));
    }

    #[test]
    fn parse_if_none_match_header() {
        let headers = "GET / HTTP/1.1\r\nHost: example\r\nIf-None-Match: \"etag\"\r\n\r\n";
        let parsed = parse_if_none_match(headers).expect("expected header value");
        assert!(if_none_match_satisfied(&parsed, "etag"));
    }

    #[test]
    fn parse_if_modified_since_header() {
        let headers = "GET / HTTP/1.1\r\nHost: x\r\nIf-Modified-Since: Fri, 15 May 2015 15:34:21 GMT\r\n\r\n";
        let t = parse_if_modified_since(headers).expect("expected date");
        let formatted = httpdate::fmt_http_date(t);
        assert_eq!(formatted, "Fri, 15 May 2015 15:34:21 GMT");
    }

    #[test]
    fn should_304_if_modified_since_same_or_later() {
        let headers = "GET / HTTP/1.1\r\nIf-Modified-Since: Fri, 15 May 2015 15:34:21 GMT\r\n\r\n";
        let file_mtime = httpdate::parse_http_date("Fri, 15 May 2015 15:34:21 GMT").ok();
        assert!(should_return_not_modified_if_modified_since("GET", headers, file_mtime));

        let file_mtime_earlier = httpdate::parse_http_date("Thu, 14 May 2015 12:00:00 GMT").ok();
        assert!(should_return_not_modified_if_modified_since("GET", headers, file_mtime_earlier));
    }

    #[test]
    fn should_not_304_if_modified_since_older() {
        let headers = "GET / HTTP/1.1\r\nIf-Modified-Since: Fri, 15 May 2015 15:34:21 GMT\r\n\r\n";
        let file_mtime = httpdate::parse_http_date("Sat, 16 May 2015 10:00:00 GMT").ok();
        assert!(!should_return_not_modified_if_modified_since("GET", headers, file_mtime));
    }

    #[test]
    fn if_modified_since_ignored_for_post() {
        let headers = "POST / HTTP/1.1\r\nIf-Modified-Since: Fri, 15 May 2015 15:34:21 GMT\r\n\r\n";
        let file_mtime = httpdate::parse_http_date("Fri, 15 May 2015 15:34:21 GMT").ok();
        assert!(!should_return_not_modified_if_modified_since("POST", headers, file_mtime));
    }
}
