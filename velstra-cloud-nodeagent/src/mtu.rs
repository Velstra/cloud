//! Does this network's MTU fit the wire this node sends it over?
//!
//! `NetworkSpec.mtu` is what every guest is told; the wire's MTU is a fact
//! about one machine. Nothing can join the two centrally — two nodes may
//! differ — so the join happens here, on the node carrying the port, and is
//! said on the port. What is *not* done is clamping: two nodes with different
//! wires would tell two guests on one segment two different MTUs, and an
//! asymmetric MTU on one L2 domain is a harder failure than the one it hides.

use velstra_cloud_model::meta::{Condition, ConditionStatus};

/// The bytes an encapsulation takes off the wire before a guest's frame fits.
/// VXLAN: outer Ethernet, IP, UDP and the VXLAN header. On the local datapath
/// there is no encapsulation at all.
pub const VXLAN_OVERHEAD: u32 = 50;

/// The condition to put on a port, or `None` when there is nothing to say.
///
/// The boundary is `<=`, on purpose and with a reason worth restating: the
/// correct default pair is a 1500-byte wire and a 1450-byte network, and that
/// pair must not trip. An alarm about nothing is the kind that teaches people
/// to ignore the real one.
pub fn condition(
    network_mtu: u32,
    wire_mtu: u32,
    overhead: u32,
    iface: &str,
    generation: u64,
) -> Option<Condition> {
    if network_mtu == 0 || wire_mtu == 0 {
        return None;
    }
    let needed = network_mtu + overhead;
    if needed <= wire_mtu {
        return None;
    }
    let fits = wire_mtu.saturating_sub(overhead);
    Some(Condition::new(
        "MtuFits",
        ConditionStatus::False,
        "TooLarge",
        &format!(
            "the network's MTU is {network_mtu}, but {iface} on this node carries {wire_mtu}\
             {}; the largest a guest here can send whole is {fits}. Large packets from this \
             guest will be dropped on the way out with nothing to see. Lower the network's MTU \
             to {fits} or less, or give this node a wider wire.",
            if overhead > 0 {
                format!(" and the overlay takes {overhead} of that")
            } else {
                String::new()
            }
        ),
        generation,
    ))
}

/// The MTU of the interface the node's default route leaves through — the
/// wire a guest's frame ends up on when this node is its first hop.
///
/// `None` when it cannot be read: a machine with no default route, or one
/// whose `ip` answers something else. Nothing is asserted then.
pub async fn uplink_mtu(ip: &str) -> Option<(String, u32)> {
    let out = tokio::process::Command::new(ip)
        .args(["-j", "route", "show", "default"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let routes: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let dev = routes
        .as_array()?
        .iter()
        .find_map(|r| r.get("dev")?.as_str().map(str::to_string))?;
    let mtu = tokio::fs::read_to_string(format!("/sys/class/net/{dev}/mtu"))
        .await
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    Some((dev, mtu))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A network asking for more than this node can carry says so** — and
    /// the correct default pair does not.
    #[test]
    fn a_network_asking_for_more_than_this_node_can_carry_says_so() {
        // 1450 on a 1450 wire over VXLAN: 1500 needed, 1450 there.
        let said = condition(1450, 1450, VXLAN_OVERHEAD, "eth0", 3).expect("a condition");
        assert_eq!(said.status, ConditionStatus::False);
        assert!(said.message.contains("1450"), "{}", said.message);
        assert!(said.message.contains("1400"), "{}", said.message);
        assert!(said.message.contains("eth0"), "{}", said.message);

        // The default pair, exactly at the boundary: silent.
        assert!(condition(1450, 1500, VXLAN_OVERHEAD, "eth0", 3).is_none());
        // No encapsulation, equal: silent.
        assert!(condition(1500, 1500, 0, "eth0", 3).is_none());
        // No encapsulation, one over: said.
        assert!(condition(1501, 1500, 0, "eth0", 3).is_some());
        // Nothing known: nothing asserted.
        assert!(condition(0, 1500, 0, "eth0", 3).is_none());
        assert!(condition(1450, 0, 0, "eth0", 3).is_none());
    }
}
