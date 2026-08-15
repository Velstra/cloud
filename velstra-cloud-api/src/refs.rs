//! How one object names another, checked at the door.
//!
//! There are two spellings in this system and both are correct, for different
//! things:
//!
//! * **A node is a bare id** — `node-a`. This is not a preference. It is what
//!   [`velstra_cloud_model::reconcile::place`] returns, what a controller writes
//!   into `spec.node`, and what an agent calls itself; and since the model
//!   started letting a node claim an object a controller assigned to it, the
//!   agent's own name is compared against that field by string equality. An
//!   object carrying `nodes/node-a` would be assigned to a node that does not
//!   answer to that name — the agent would not be refused loudly, it would
//!   simply never be the owner, and nothing would ever start.
//! * **Everything else is a full resource name** — `projects/p1/images/…`.
//!   These are followed: something has to read them and go and fetch the
//!   object, and a bare id under an unstated parent is not enough to find one.
//!
//! Both spellings are checked here rather than written down somewhere, because
//! the failure they cause is silent on both sides: the console cannot tell that
//! its attachment is being ignored, and the agent cannot tell that an object
//! was meant for it.

use serde_json::Value;
use velstra_cloud_model::meta::ResourceName;

use crate::{
    error::{ApiError, ApiResult},
    json::to_camel,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    /// `node-a` — an id on its own.
    BareId,
    /// `projects/p1/volumes/v1` — collection/id pairs, all the way up.
    Name,
}

/// The reference fields of each collection, in the model's spelling.
///
/// Only fields whose form the model states: a field that could plausibly be
/// either is left alone rather than guessed at, because refusing a spelling
/// somebody was told to use is worse than not checking it.
fn fields(kind: &str) -> &'static [(&'static str, Form)] {
    match kind {
        "instances" => &[
            ("node", Form::BareId),
            ("image", Form::Name),
            ("ports", Form::Name),
        ],
        "attachments" => &[
            ("node", Form::BareId),
            ("volume", Form::Name),
            ("instance", Form::Name),
        ],
        "migrations" => &[
            ("instance", Form::Name),
            ("from_node", Form::BareId),
            ("to_node", Form::BareId),
        ],
        "volumes" => &[("source_image", Form::Name)],
        "ports" => &[("network", Form::Name), ("subnet", Form::Name)],
        "subnets" => &[("network", Form::Name)],
        "projects" => &[("parent", Form::Name)],
        _ => &[],
    }
}

/// Check every reference in a spec — whole or partial, since a change carries
/// only what it changes and that is exactly what needs checking.
pub fn check(kind: &str, spec: &Value) -> ApiResult<()> {
    for (field, form) in fields(kind) {
        let Some(value) = spec.get(field) else {
            continue;
        };
        match value {
            Value::String(one) => one_of(one, *form, &format!("spec.{}", to_camel(field)))?,
            Value::Array(many) => {
                for (i, item) in many.iter().enumerate() {
                    if let Value::String(one) = item {
                        one_of(one, *form, &format!("spec.{}[{i}]", to_camel(field)))?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn one_of(value: &str, form: Form, field: &str) -> ApiResult<()> {
    // An empty reference is an unset one. A machine with no image yet is a
    // perfectly ordinary thing to create.
    if value.is_empty() {
        return Ok(());
    }
    match form {
        Form::BareId => {
            if ResourceName::parse(&format!("nodes/{value}")).is_err() {
                return Err(ApiError::invalid(format!(
                    "a node is named by its id — `node-a`, not `{value}`: that is the name the \
                     scheduler writes and the name an agent answers to"
                ))
                .at(field));
            }
        }
        Form::Name => {
            if ResourceName::parse(value).is_err() {
                return Err(ApiError::invalid(format!(
                    "`{value}` is not a resource name: give the whole thing, like \
                     `projects/p1/images/sha256-3f9a2b`, because something has to follow it"
                ))
                .at(field));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_node_is_a_bare_id_and_a_full_name_is_refused() {
        // The whole point: this refusal is loud, and the alternative is an
        // object assigned to a node that never answers to that name.
        let refused = check("instances", &json!({ "node": "nodes/node-a" })).unwrap_err();
        assert_eq!(refused.field.as_deref(), Some("spec.node"));
        assert!(refused.message.contains("node-a"));
        assert!(check("instances", &json!({ "node": "node-a" })).is_ok());
        assert!(check("attachments", &json!({ "node": "node-a" })).is_ok());
    }

    #[test]
    fn everything_else_is_a_full_name_and_a_bare_id_is_refused() {
        let refused = check("instances", &json!({ "image": "sha256-3f9a2b" })).unwrap_err();
        assert_eq!(refused.field.as_deref(), Some("spec.image"));
        assert!(
            check(
                "instances",
                &json!({ "image": "projects/p1/images/sha256-3f9a2b" })
            )
            .is_ok()
        );
    }

    #[test]
    fn a_list_of_references_says_which_one_is_wrong() {
        // "one of your ports is wrong" is a refusal an operator has to bisect
        // by hand.
        let refused = check(
            "instances",
            &json!({ "ports": ["projects/p1/ports/port-a", "port-b"] }),
        )
        .unwrap_err();
        assert_eq!(refused.field.as_deref(), Some("spec.ports[1]"));
    }

    #[test]
    fn an_unset_reference_is_not_a_wrong_one() {
        // A machine with no image yet, or no node yet, is ordinary — and a
        // create that filled a spec from its defaults sends exactly that.
        assert!(
            check(
                "instances",
                &json!({ "node": "", "image": "", "ports": [] })
            )
            .is_ok()
        );
        assert!(check("instances", &json!({ "node": null })).is_ok());
        assert!(check("instances", &json!({})).is_ok());
    }

    #[test]
    fn a_collection_with_nothing_to_follow_is_left_alone() {
        assert!(check("networks", &json!({ "vni": 5001 })).is_ok());
        assert!(check("nodes", &json!({ "schedulable": true })).is_ok());
    }
}
