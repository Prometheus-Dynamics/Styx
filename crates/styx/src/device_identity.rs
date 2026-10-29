/// The key of a USB camera's physical device: its vendor:product on its bus port (`bus`, the
/// V4L2 `bus_info` such as `usb-xhci-hcd.1-1.1.1`, which every node of the camera shares).
///
/// Two identical cameras without serials share a vendor:product, and `uvcvideo` and the card
/// name are shared by every UVC camera, so none of those identify a camera. The port does.
pub(crate) fn usb_key(bus: &str, vendor: &str, product: &str) -> String {
    format!("{}:{}@{}", vendor.trim(), product.trim(), bus.trim())
}

/// The keys of the device a node or backend belongs to. A USB camera's keys name only that
/// camera: its bus port and its vendor:product on that port. Other devices get the properties
/// that identify them (bus, model, vendor:product) as before.
pub(crate) fn derive_keys(id: &str, props: &[(String, String)]) -> Vec<String> {
    if let Some(bus) = prop(props, "bus").filter(|bus| bus.starts_with("usb")) {
        let mut keys = vec![bus.to_string()];
        if let (Some(vendor), Some(product)) =
            (prop(props, "usb_vendor"), prop(props, "usb_product"))
        {
            keys.push(usb_key(bus, vendor, product));
        }
        return keys;
    }
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
    // The node's own card name before its bus port: a USB camera's port is no name for it.
    if let Some(card) = prop(props, "card") {
        return card.to_string();
    }
    if let Some(bus) = prop(props, "bus") {
        return bus.to_string();
    }
    id.to_string()
}

/// The first non-empty value of a property, by name (ASCII case-insensitive), trimmed.
fn prop<'a>(props: &'a [(String, String)], name: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
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

#[cfg(test)]
#[path = "device_identity_tests.rs"]
mod tests;
