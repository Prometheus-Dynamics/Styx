pub(crate) fn derive_keys(id: &str, props: &[(String, String)]) -> Vec<String> {
    let mut keys = Vec::new();
    if !id.starts_with("/dev/video") {
        keys.push(id.to_string());
    }
    for (k, v) in props {
        let v_trimmed = v.trim();
        let v_lower = v_trimmed.to_ascii_lowercase();
        if v_lower == "rp1-cfe" {
            continue;
        }
        if is_identity_property(k) {
            keys.push(v_trimmed.to_string());
        }
        if let Some(vidpid) = extract_vid_pid(v_trimmed) {
            keys.push(vidpid);
        }
    }
    if let Some(vidpid) = extract_vid_pid(id) {
        keys.push(vidpid);
    }
    keys
}

#[cfg(any(feature = "v4l2", feature = "libcamera"))]
pub(crate) fn pick_display_id(id: &str, props: &[(String, String)]) -> String {
    if let Some(model) = props
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("model"))
        .map(|(_, v)| v.trim())
        && !model.is_empty()
        && !model.eq_ignore_ascii_case("rp1-cfe")
    {
        return model.to_string();
    }
    if let Some(vidpid) = props.iter().find_map(|(_, v)| extract_vid_pid(v)) {
        return vidpid;
    }
    if let Some(bus) = props
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("bus"))
        .map(|(_, v)| v.clone())
    {
        return bus;
    }
    if let Some(card) = props
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("card"))
        .map(|(_, v)| v.clone())
    {
        return card;
    }
    id.to_string()
}

fn is_identity_property(key: &str) -> bool {
    key.eq_ignore_ascii_case("bus")
        || key.eq_ignore_ascii_case("card")
        || key.eq_ignore_ascii_case("driver")
        || key.eq_ignore_ascii_case("model")
}

fn extract_vid_pid(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(8) {
        let slice = &bytes[i..i + 9];
        if slice[4] != b':' {
            continue;
        }
        if slice[..4].iter().all(|b| b.is_ascii_hexdigit())
            && slice[5..].iter().all(|b| b.is_ascii_hexdigit())
        {
            return Some(String::from_utf8_lossy(slice).to_string());
        }
    }
    None
}
