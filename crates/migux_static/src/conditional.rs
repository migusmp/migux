#[derive(Debug)]
enum IfNoneMatch {
    Any,
    Tags(Vec<String>),
}

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

#[cfg(test)]
mod tests {
    use super::{
        IfNoneMatch, if_none_match_satisfied, parse_if_none_match, parse_if_none_match_value,
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
}
