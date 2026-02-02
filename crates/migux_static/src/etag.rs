use std::{fs::Metadata, time::UNIX_EPOCH};

pub struct EtagInfo {
    pub value: String,
    pub header: String,
    pub mtime_nanos: u128,
}

pub fn weak_etag_size_mtime(metadata: &Metadata) -> EtagInfo {
    let size = metadata.len();
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|dur| dur.as_nanos())
        .unwrap_or(0);

    let value = format!("{size}-{mtime_nanos}");
    let header = format!(r#"W/"{value}""#);

    EtagInfo {
        value,
        header,
        mtime_nanos,
    }
}

pub fn last_modified_header(metadata: &Metadata) -> Option<String> {
    metadata.modified().ok().map(httpdate::fmt_http_date) // -> String RFC1123 en GMT
}
