//! Shared destination-address admission policy for outbound network paths.

use std::net::IpAddr;

/// Reports whether an address is admitted as an ordinary public destination.
pub fn is_public_destination_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, third, _fourth] = address.octets();
            !(first == 0
                || first == 10
                || first == 127
                || first >= 224
                || (first == 100 && (64..=127).contains(&second))
                || (first == 169 && second == 254)
                || (first == 172 && (16..=31).contains(&second))
                || (first == 192 && second == 0 && third == 0)
                || (first == 192 && second == 0 && third == 2)
                || (first == 192 && second == 88 && third == 99)
                || (first == 192 && second == 168)
                || (first == 198 && matches!(second, 18 | 19))
                || (first == 198 && second == 51 && third == 100)
                || (first == 203 && second == 0 && third == 113))
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            let in_global_unicast = (0x2000..=0x3fff).contains(&segments[0]);
            let special_2001 =
                segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8);
            let transition_6to4 = segments[0] == 0x2002;
            let documentation_3fff = segments[0] == 0x3fff && segments[1] <= 0x0fff;
            in_global_unicast && !special_2001 && !transition_6to4 && !documentation_3fff
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_unicast_destinations_are_public() {
        let public_v4 = "93.184.216.34".parse().expect("fixture IPv4 parses");
        let public_v6 = "2606:2800:220:1:248:1893:25c8:1946"
            .parse()
            .expect("fixture IPv6 parses");

        assert!(is_public_destination_address(public_v4));
        assert!(is_public_destination_address(public_v6));
    }

    #[test]
    fn non_public_destination_ranges_are_rejected() {
        let private_v4 = "10.0.0.1".parse().expect("fixture IPv4 parses");
        let shared_v4 = "100.64.0.1".parse().expect("fixture IPv4 parses");
        let loopback_v6 = "::1".parse().expect("fixture IPv6 parses");
        let documentation_v6 = "2001:db8::1".parse().expect("fixture IPv6 parses");

        assert!(!is_public_destination_address(private_v4));
        assert!(!is_public_destination_address(shared_v4));
        assert!(!is_public_destination_address(loopback_v6));
        assert!(!is_public_destination_address(documentation_v6));
    }
}
