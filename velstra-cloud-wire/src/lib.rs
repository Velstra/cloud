//! The JSON the contract promises, from the model as it is.
//!
//! `docs/rest-contract.md` spells its fields `observedGeneration` and
//! `createdAt`, and the model crate spells them `observed_generation` and
//! `created_at`. That crate is shared and is not mine to annotate, so the
//! rename happens here, once, for every type at the same time — which also
//! means a field added to the model needs no edit in this file to reach the
//! wire correctly.
//!
//! Two things are not mechanical, and both are here rather than scattered:
//!
//! * **`labels` holds somebody else's keys.** A label called `cost_center` is
//!   data, not a field name, and renaming it to `costCenter` would corrupt what
//!   an operator typed. The transform stops at the boundary of a labels object.
//! * **`revision` is a string on the wire.** It is opaque and clients must not
//!   order or increment it; a JSON number is an invitation to do both.
//! * **A name is one string.** The model keeps a name parsed — it needs the
//!   parent and the collection far more often than the whole thing — and
//!   serialises as `{"segments": […]}`. The contract spells it
//!   `projects/p1/instances/i1`, which is also what a URL, a log line and a
//!   person all use, so the join happens here and its inverse happens on the
//!   way in.

use serde_json::{Map, Value};

/// Model shape in, contract shape out.
pub fn to_wire(value: Value) -> Value {
    convert(value, Case::Camel, false)
}

/// Contract shape in, model shape out.
pub fn from_wire(value: Value) -> Value {
    convert(value, Case::Snake, false)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Camel,
    Snake,
}

fn convert(value: Value, case: Case, opaque_keys: bool) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, v) in map {
                let is_labels = key == "labels";
                let name = if opaque_keys {
                    key
                } else {
                    match case {
                        Case::Camel => to_camel(&key),
                        Case::Snake => to_snake(&key),
                    }
                };
                let v = match name.as_str() {
                    _ if opaque_keys => convert(v, case, true),
                    "revision" => revision(v, case),
                    "name" => resource_name(v, case),
                    _ => convert(v, case, is_labels),
                };
                out.insert(name, v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| convert(v, case, opaque_keys))
                .collect(),
        ),
        other => other,
    }
}

/// A revision is a number in the store and a string on the wire. Anything else
/// is passed through untouched so a malformed one is rejected by the model's
/// own deserialisation, with its own message, rather than here.
fn revision(v: Value, case: Case) -> Value {
    match (case, v) {
        (Case::Camel, Value::Number(n)) => Value::String(n.to_string()),
        (Case::Snake, Value::String(s)) => match s.parse::<u64>() {
            Ok(n) => Value::Number(n.into()),
            Err(_) => Value::String(s),
        },
        (_, other) => other,
    }
}

/// A parsed name in the store, one string on the wire.
fn resource_name(v: Value, case: Case) -> Value {
    match (case, v) {
        (Case::Camel, Value::Object(map)) => match joined(&Value::Object(map.clone())) {
            Some(name) => Value::String(name),
            None => Value::Object(map),
        },
        (Case::Snake, Value::String(name)) => json_segments(&name),
        (_, other) => other,
    }
}

fn json_segments(name: &str) -> Value {
    Value::Object(Map::from_iter([(
        "segments".to_string(),
        Value::Array(
            name.split('/')
                .map(|s| Value::String(s.to_string()))
                .collect(),
        ),
    )]))
}

/// Read a name whichever way round it is: a string from a client, the parsed
/// form from the store. Anything that needs a name as text goes through here so
/// there is one answer to "which shape is it in".
pub fn joined(value: &Value) -> Option<String> {
    if let Some(name) = value.as_str() {
        return Some(name.to_string());
    }
    let segments = value.get("segments")?.as_array()?;
    let parts: Vec<&str> = segments.iter().filter_map(Value::as_str).collect();
    if parts.len() != segments.len() {
        return None;
    }
    Some(parts.join("/"))
}

pub fn to_camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper_next = false;
    for c in key.chars() {
        if c == '_' {
            upper_next = true;
            continue;
        }
        if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn to_snake(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let mut out = String::with_capacity(key.len() + 2);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            out.push('_');
        } else if c.is_ascii_digit() && i > 0 && chars[i - 1].is_alphabetic() {
            // A digit after a letter is ambiguous on its own: `hugepages1gi`
            // came from `hugepages_1gi`, and `l3Vni` came from `l3_vni`. Camel
            // case does distinguish them, though — what follows the digits does.
            //
            // `hugepages1gi`: lowercase after the digits, so the digits started
            // a new word and there was an underscore before them.
            // `l3Vni`: an uppercase after the digits, so the digits belong to
            // the word before them and the uppercase is the next boundary.
            //
            // Without this the round trip is not a round trip: `l3_vni` came
            // back as `l_3_vni`, and a field that does not survive its own wire
            // is a field a client cannot write.
            let next_is_upper = chars[i + 1..]
                .iter()
                .find(|n| !n.is_ascii_digit())
                .is_some_and(|n| n.is_ascii_uppercase());
            if !next_is_upper {
                out.push('_');
            }
        }
        out.extend(c.to_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use velstra_cloud_model::{
        meta::{Condition, ConditionStatus, Meta, Placement, ResourceName, Revision, Timestamp},
        resources::{Capacity, NodeSpec, NodeStatus, Resource},
    };

    use super::*;

    /// Every model type must survive model → wire → model unchanged. This is
    /// the test that catches a name whose two transforms are not each other's
    /// inverse, which would otherwise show up as a field that quietly resets
    /// itself on the next write.
    fn round_trip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let wire = to_wire(serde_json::to_value(value).unwrap());
        let back: T = serde_json::from_value(from_wire(wire)).unwrap();
        assert_eq!(&back, value);
    }

    #[test]
    fn a_node_survives_the_wire_including_its_hugepages() {
        let mut meta = Meta::new(
            ResourceName::parse("nodes/node-a").unwrap(),
            Placement::new("eu-central", "cell-1"),
        );
        meta.labels.insert("cost_center".into(), "r_and_d".into());
        let node = Resource::new(
            meta,
            NodeSpec {
                schedulable: true,
                labels: vec!["ssd".into()],
            },
            NodeStatus {
                observed_generation: 3,
                conditions: vec![Condition::new(
                    "Ready",
                    ConditionStatus::True,
                    "Ready",
                    "",
                    3,
                )],
                capacity: Capacity {
                    vcpus: 64,
                    memory_mib: 262144,
                    disk_gib: 4000,
                    numa_free_mib: vec![131072, 131072],
                    hugepages_1gi: 16,
                },
                allocated: Capacity::default(),
                agent_version: "0.1.0".into(),
                last_heartbeat: Timestamp(1786732800000),
                images: vec!["projects/p1/images/sha256-abc".into()],
                // A busy disk, because the *reason* is what the console shows —
                // and this test is about the wire keeping camelCase and the
                // nested shape, which a tagged enum is the easiest thing to get
                // wrong.
                devices: vec![velstra_cloud_model::ceph::BlockDevice {
                    path: "/dev/disk/by-id/wwn-0x5000".into(),
                    kernel_name: "sdb".into(),
                    size_gib: 500,
                    rotational: true,
                    model: "ST500".into(),
                    serial: "X1".into(),
                    state: velstra_cloud_model::ceph::DeviceUse::Filesystem {
                        fstype: "ext4".into(),
                    },
                }],
                ceph: None,
            },
        );
        round_trip(&node);
    }

    #[test]
    fn the_wire_spells_fields_the_way_the_contract_does() {
        let mut meta = Meta::new(
            ResourceName::parse("projects/p1/instances/i1").unwrap(),
            Placement::new("eu", "cell-1"),
        );
        meta.revision = Revision(412);
        let wire = to_wire(serde_json::to_value(&meta).unwrap());
        assert_eq!(wire["createdAt"], json!(meta.created_at.0));
        assert_eq!(wire["deletedAt"], Value::Null);
        // A string, because a client that can add one to a revision will.
        assert_eq!(wire["revision"], json!("412"));
    }

    #[test]
    fn a_label_an_operator_typed_is_data_and_is_left_alone() {
        let value =
            json!({ "meta": { "labels": { "cost_center": "r_and_d", "teamName": "net" } } });
        let wire = to_wire(value.clone());
        assert_eq!(wire["meta"]["labels"]["cost_center"], json!("r_and_d"));
        assert_eq!(wire["meta"]["labels"]["teamName"], json!("net"));
        assert_eq!(
            from_wire(wire),
            value,
            "a label key was rewritten as if it were a field"
        );
    }

    #[test]
    fn a_conditions_list_is_converted_element_by_element() {
        let wire = to_wire(
            json!({ "conditions": [ { "observed_generation": 2, "last_transition": 7 } ] }),
        );
        assert_eq!(wire["conditions"][0]["observedGeneration"], json!(2));
        assert_eq!(wire["conditions"][0]["lastTransition"], json!(7));
    }
}
