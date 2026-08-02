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
    let port = port.parse::<u16>().ok()?;
    Some(format!("{retained_prefix}{port}"))
}

pub(super) fn discarded_port_zero_prefix_context(value: &str) -> Option<&str> {
    let retained_context = value.trim_end_matches('0');
    if retained_context.len() == value.len()
        || (!retained_context.is_empty() && !retained_context.ends_with(':'))
    {
        return None;
    }
    Some(retained_context)
}

pub(super) fn canonicalized_url_host(value: &str) -> Option<String> {
    let candidate = format!("http://{value}/");
    let url = Url::parse(&candidate).ok()?;
    (url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && !has_explicit_authority_port(value))
    .then(|| url.host_str().map(str::to_owned))?
}

fn has_explicit_authority_port(value: &str) -> bool {
    if let Some(suffix) = value
        .strip_prefix('[')
        .and_then(|remainder| remainder.split_once(']').map(|(_, suffix)| suffix))
    {
        return suffix.starts_with(':');
    }
    value.contains(':')
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
        .trim_matches(|character: char| character <= '\u{1f}' || character == ' ')
        .chars()
        .filter(|character| !matches!(character, '\t' | '\n' | '\r'))
        .collect::<String>();
    (normalized != value && !normalized.is_empty()).then_some(normalized)
}

pub(super) fn idna_mapped_unicode_variant(value: &str) -> Option<String> {
    let ascii = idna::domain_to_ascii(value).ok()?;
    let (unicode, decoding) = idna::domain_to_unicode(&ascii);
    decoding.ok()?;
    (!unicode.is_empty() && unicode != value).then_some(unicode)
}

pub(super) fn canonicalized_ipv4_component_fragments(value: &str) -> impl Iterator<Item = Vec<u8>> {
    let mut spellings = vec![value.to_owned()];
    if value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        spellings.push(format!("0x{value}"));
    }
    if value.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
        spellings.push(format!("0{value}"));
    }
    let mut fragments = Vec::new();
    for spelling in spellings {
        fragments.extend(
            [
                canonicalized_ipv4_fragment(&format!("{spelling}.0.0.1"), 0..1),
                canonicalized_ipv4_fragment(&format!("0.{spelling}.0.1"), 1..2),
                canonicalized_ipv4_fragment(&format!("0.0.{spelling}.1"), 2..3),
                canonicalized_ipv4_fragment(&format!("0.0.0.{spelling}"), 3..4),
                canonicalized_ipv4_fragment(&format!("0.0.{spelling}"), 2..4),
                canonicalized_ipv4_fragment(&format!("0.{spelling}"), 1..4),
                canonicalized_ipv4_fragment(&spelling, 0..4),
            ]
            .into_iter()
            .flatten(),
        );
    }
    fragments.into_iter()
}

pub(super) fn legacy_ipv4_component_contains(value: &str, address: std::net::Ipv4Addr) -> bool {
    let octets = address.octets();
    let whole_address = u32::from_be_bytes(octets);
    let final_three = u32::from_be_bytes([0, octets[1], octets[2], octets[3]]);
    let final_two = u32::from_be_bytes([0, 0, octets[2], octets[3]]);
    let component_values = [
        whole_address,
        u32::from(octets[0]),
        final_three,
        u32::from(octets[0]),
        u32::from(octets[1]),
        final_two,
        u32::from(octets[0]),
        u32::from(octets[1]),
        u32::from(octets[2]),
        u32::from(octets[3]),
    ];
    let lowercase = value.to_ascii_lowercase();
    legacy_ipv4_address_spellings(address)
        .iter()
        .any(|spelling| spelling.contains(&lowercase))
        || ipv4_radix_padding_may_contain(value)
        || normalized_radix_fragment(value, 10, None).is_some_and(|fragment| {
            component_values
                .iter()
                .any(|component| component.to_string().contains(&fragment))
        })
        || normalized_radix_fragment(value, 8, None).is_some_and(|fragment| {
            component_values
                .iter()
                .any(|component| format!("{component:o}").contains(&fragment))
        })
        || normalized_radix_fragment(value, 16, Some("0x")).is_some_and(|fragment| {
            component_values
                .iter()
                .any(|component| format!("{component:x}").contains(&fragment))
        })
        || value
            .strip_prefix('x')
            .or_else(|| value.strip_prefix('X'))
            .and_then(|digits| normalized_radix_fragment(digits, 16, None))
            .is_some_and(|fragment| {
                component_values
                    .iter()
                    .any(|component| format!("{component:x}").contains(&fragment))
            })
}

fn legacy_ipv4_address_spellings(address: std::net::Ipv4Addr) -> Vec<String> {
    let octets = address.octets();
    let whole_address = u32::from_be_bytes(octets);
    let final_three = u32::from_be_bytes([0, octets[1], octets[2], octets[3]]);
    let final_two = u32::from_be_bytes([0, 0, octets[2], octets[3]]);
    let component_layouts = [
        vec![whole_address],
        vec![u32::from(octets[0]), final_three],
        vec![u32::from(octets[0]), u32::from(octets[1]), final_two],
        octets.into_iter().map(u32::from).collect(),
    ];
    let mut addresses = Vec::new();
    for layout in component_layouts {
        let mut spellings = vec![String::new()];
        for component in layout {
            let representations = [
                component.to_string(),
                format!("0{component:o}"),
                format!("0x{component:x}"),
            ];
            let mut expanded = Vec::with_capacity(spellings.len() * representations.len());
            for prefix in spellings {
                for representation in &representations {
                    expanded.push(if prefix.is_empty() {
                        representation.clone()
                    } else {
                        format!("{prefix}.{representation}")
                    });
                }
            }
            spellings = expanded;
        }
        addresses.extend(spellings);
    }
    addresses
}

pub(super) fn ipv4_radix_padding_may_contain(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    let digits = lowercase
        .strip_prefix("0x")
        .or_else(|| lowercase.strip_prefix('x'))
        .unwrap_or(&lowercase);
    !digits.is_empty() && digits.bytes().all(|byte| byte == b'0')
}

pub(super) fn normalized_radix_fragment(
    value: &str,
    radix: u32,
    prefix: Option<&str>,
) -> Option<String> {
    let digits = prefix
        .and_then(|prefix| {
            value
                .strip_prefix(prefix)
                .or_else(|| value.strip_prefix(&prefix.to_ascii_uppercase()))
        })
        .unwrap_or(value);
    if digits.is_empty() || !digits.chars().all(|character| character.is_digit(radix)) {
        return None;
    }
    let normalized = digits.trim_start_matches('0');
    Some(if normalized.is_empty() {
        String::from("0")
    } else {
        normalized.to_ascii_lowercase()
    })
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

pub(super) fn canonicalized_ipv6_hextet_text_fragment(value: &str) -> Option<String> {
    let value = value.strip_prefix('[').unwrap_or(value);
    let value = value.strip_suffix(']').unwrap_or(value);
    if value.is_empty()
        || value.len() > 4
        || value.contains(':')
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let hextet = u16::from_str_radix(value, 16).ok()?;
    Some(format!("{hextet:x}"))
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
