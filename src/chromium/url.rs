pub fn normalize_url(raw: &str) -> String {
    let u = raw.trim();
    if u.contains("://") || u.starts_with("about:") || u.starts_with("data:") {
        u.to_string()
    } else if !u.contains(' ')
        && (u.contains('.') || u.starts_with("localhost"))
        && !u.ends_with('.')
    {
        if u.starts_with("localhost") || u.starts_with("127.0.0.1") {
            format!("http://{u}")
        } else {
            format!("https://{u}")
        }
    } else {
        let q: String = u
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".into(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        crate::migration::search::query_url(&q)
    }
}
