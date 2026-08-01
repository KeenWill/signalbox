use reqwest::Url;

pub(super) fn normalize_url_path_dot_segments(path: &str) -> Option<String> {
    let slash_normalized = path.replace('\\', "/");
    let retain_trailing_separator =
        slash_normalized.ends_with("/.") || slash_normalized.ends_with("/..");
    let mut normalized_segments: Vec<&str> = Vec::new();
    let mut changed = slash_normalized != path;
    for segment in slash_normalized.split('/') {
        match segment {
            "." => changed = true,
            ".." => {
                changed = true;
                if normalized_segments
                    .last()
                    .is_some_and(|prior| !prior.is_empty())
                {
                    normalized_segments.pop();
                }
            }
            _ => normalized_segments.push(segment),
        }
    }
    let mut normalized = normalized_segments.join("/");
    if retain_trailing_separator && !normalized.ends_with('/') {
        normalized.push('/');
    }
    (changed && !normalized.is_empty() && normalized != slash_normalized).then_some(normalized)
}

pub(super) fn canonicalized_url_port_fragment(value: &str) -> Option<String> {
    let (port, retained_prefix) = match value.strip_prefix(':') {
        Some(port) => (port, ":"),
        None => (value, ""),
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let candidate = format!("http://example.com:{port}/");
    let url = Url::parse(&candidate).ok()?;
    url.port().map(|port| format!("{retained_prefix}{port}"))
}

pub(super) fn canonicalized_url_host(value: &str) -> Option<String> {
    let candidate = format!("http://{value}/");
    let url = Url::parse(&candidate).ok()?;
    (url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| url.host_str().map(str::to_owned))?
}

pub(super) fn canonicalized_complete_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| String::from(url.as_str()))
}

pub(super) fn parse_ip_literal(value: &str) -> Option<std::net::IpAddr> {
    let unbracketed = value
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(value);
    unbracketed.parse().ok()
}

pub(super) fn url_preprocessed_credential_variant(value: &str) -> Option<String> {
    let normalized = value
        .chars()
        .filter(|character| !matches!(character, '\t' | '\n' | '\r'))
        .collect::<String>();
    (normalized != value && !normalized.is_empty()).then_some(normalized)
}

pub(super) fn canonicalized_ipv4_component_fragments(value: &str) -> impl Iterator<Item = Vec<u8>> {
    [
        canonicalized_ipv4_fragment(&format!("{value}.0.0.1"), 0..1),
        canonicalized_ipv4_fragment(&format!("0.{value}.0.1"), 1..2),
        canonicalized_ipv4_fragment(&format!("0.0.{value}.1"), 2..3),
        canonicalized_ipv4_fragment(&format!("0.0.0.{value}"), 3..4),
        canonicalized_ipv4_fragment(&format!("0.0.{value}"), 2..4),
        canonicalized_ipv4_fragment(&format!("0.{value}"), 1..4),
        canonicalized_ipv4_fragment(value, 0..4),
    ]
    .into_iter()
    .flatten()
}

pub(super) fn canonicalized_ipv4_fragment(
    host: &str,
    positions: std::ops::Range<usize>,
) -> Option<Vec<u8>> {
    let candidate = format!("http://{host}/");
    let url = Url::parse(&candidate).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let components = url.host_str()?.parse::<std::net::Ipv4Addr>().ok()?.octets();
    components.get(positions).map(<[u8]>::to_vec)
}

pub(super) fn canonicalized_ipv6_fragments(value: &str) -> Vec<Vec<u16>> {
    let value = value.strip_prefix('[').unwrap_or(value);
    let value = value.strip_suffix(']').unwrap_or(value);
    if value.is_empty() {
        return Vec::new();
    }
    let has_compression = value.contains("::");
    let explicit_component_count = value
        .split(':')
        .filter(|component| !component.is_empty())
        .count();
    if explicit_component_count > 8
        || (!has_compression && value.split(':').any(str::is_empty))
        || (has_compression && explicit_component_count >= 8)
    {
        return Vec::new();
    }
    let minimum_component_count = explicit_component_count + usize::from(has_compression);
    let maximum_component_count = if has_compression {
        8
    } else {
        minimum_component_count
    };
    let mut fragments = Vec::new();
    for component_count in minimum_component_count..=maximum_component_count {
        for start in 0..=8 - component_count {
            let host = embedded_ipv6_fragment_candidate(value, start, 8 - start - component_count);
            if let Some(fragment) =
                canonicalized_ipv6_fragment(&host, start..start + component_count)
            {
                fragments.push(fragment);
            }
        }
    }
    fragments
}

pub(super) fn embedded_ipv6_fragment_candidate(
    value: &str,
    leading_components: usize,
    trailing_components: usize,
) -> String {
    let mut host = String::new();
    for _ in 0..leading_components {
        if !host.is_empty() {
            host.push(':');
        }
        host.push('0');
    }
    if !host.is_empty() && !value.starts_with(':') {
        host.push(':');
    }
    host.push_str(value);
    for _ in 0..trailing_components {
        if !host.ends_with(':') {
            host.push(':');
        }
        host.push('0');
    }
    host
}

pub(super) fn canonicalized_ipv6_fragment(
    host: &str,
    positions: std::ops::Range<usize>,
) -> Option<Vec<u16>> {
    let candidate = format!("http://[{host}]/");
    let url = Url::parse(&candidate).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let std::net::IpAddr::V6(address) = parse_ip_literal(url.host_str()?)? else {
        return None;
    };
    address.segments().get(positions).map(<[u16]>::to_vec)
}

pub(super) fn canonicalized_ipv4_tail_fragments(value: &str) -> Vec<Vec<u8>> {
    let component_count = value.split('.').count();
    if component_count > 4 || value.split('.').any(str::is_empty) {
        return Vec::new();
    }
    let mut fragments = Vec::new();
    for start in 0..=4 - component_count {
        let prefix = "0.".repeat(start);
        let suffix = ".0".repeat(4 - start - component_count);
        let tail = format!("{prefix}{value}{suffix}");
        if let Some(fragment) =
            canonicalized_ipv4_tail_fragment(&tail, start..start + component_count)
        {
            fragments.push(fragment);
        }
    }
    fragments
}

pub(super) fn canonicalized_mixed_ipv6_ipv4_tail_fragments(
    value: &str,
) -> (Vec<Vec<u16>>, Vec<Vec<u8>>) {
    let Some((ipv6_prefix, ipv4_tail)) = value.rsplit_once(':') else {
        return (Vec::new(), Vec::new());
    };
    if !ipv4_tail.contains('.')
        || (!ipv6_prefix.is_empty()
            && canonicalized_ipv6_fragments(&format!("{ipv6_prefix}:0")).is_empty())
    {
        return (Vec::new(), Vec::new());
    }
    let octet_fragments = canonicalized_ipv4_tail_fragments(ipv4_tail);
    let mut component_fragments = Vec::new();
    for octets in &octet_fragments {
        if !octets.len().is_multiple_of(2) {
            continue;
        }
        let canonical_tail = octets
            .chunks_exact(2)
            .map(|pair| format!("{:x}", u16::from_be_bytes([pair[0], pair[1]])))
            .collect::<Vec<_>>()
            .join(":");
        component_fragments.extend(canonicalized_ipv6_fragments(&format!(
            "{ipv6_prefix}:{canonical_tail}"
        )));
    }
    (component_fragments, octet_fragments)
}

pub(super) fn canonicalized_ipv4_tail_fragment(
    tail: &str,
    positions: std::ops::Range<usize>,
) -> Option<Vec<u8>> {
    let candidate = format!("http://[::ffff:{tail}]/");
    let url = Url::parse(&candidate).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let std::net::IpAddr::V6(address) = parse_ip_literal(url.host_str()?)? else {
        return None;
    };
    let positions = positions.start + 12..positions.end + 12;
    address.octets().get(positions).map(<[u8]>::to_vec)
}
