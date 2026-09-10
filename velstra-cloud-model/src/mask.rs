//! Which fields of an update actually change.
//!
//! A REST client sends what it wants changed, and everything it leaves out
//! stays as it is — that is what `PATCH` means. A gRPC client cannot: proto3
//! has no absent scalar. A field the caller never touched arrives at its zero
//! value and is byte-for-byte the same as one they deliberately set to zero.
//!
//! So an update carrying a whole message means "make the object look like
//! this", and changing a guest's vCPU count also writes its cloud-init, its
//! ports and its placement policy with whatever the client happened to be
//! holding — which, for a client that read the object a minute ago, is a
//! minute-old copy of somebody else's edit.
//!
//! A **field mask** is the answer, and it is the same one AIP-134 gives: the
//! request names the paths it changes, and nothing else is touched.
//!
//! Two decisions worth stating:
//!
//! * **An empty mask means everything.** That is the behaviour every existing
//!   client already gets, so adding this breaks nobody — and it is also the
//!   reason to send a mask, which the field's own documentation says.
//! * **A path nobody has is refused**, not ignored. A caller who names
//!   `spec.vcpu` and is answered `200` has been told their change was made.
//!   This is the same rule the REST surface applies to an unknown field.

use serde_json::Value;

/// The path that means "everything in this message".
pub const EVERYTHING: &str = "*";

/// A path in a mask that names nothing on the object.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "update_mask names {path}, which is not a field of a {kind}. A mask that names nothing is a \
     change nobody would make, and answering OK to one would say it had been made."
)]
pub struct NoSuchPath {
    pub path: String,
    pub kind: String,
}

/// Keep only what the mask names.
///
/// `whole` is the update as the request carries it — `{"spec": {...},
/// "meta": {"labels": {...}}}` — and the paths are relative to the resource,
/// so `spec.vcpus` and `meta.labels` are what a caller writes.
///
/// An empty mask, or one containing [`EVERYTHING`], hands back `whole`
/// unchanged.
///
/// A path that names a *branch* (`spec`) keeps the whole branch, which is what
/// AIP-134 says a parent path means and what somebody writing it expects.
///
/// Both spellings of a step are accepted — `spec.memory_mib` and
/// `spec.memoryMib`. The proto declares fields in snake_case and AIP-134 says
/// a mask uses those names, but a generated client in a language whose JSON
/// mapping is lowerCamelCase will hand its user the camel spelling, and there
/// is no reading under which one of the two is a typo.
pub fn apply(whole: &Value, paths: &[String], kind: &str) -> Result<Value, NoSuchPath> {
    if paths.is_empty() || paths.iter().any(|p| p == EVERYTHING) {
        return Ok(whole.clone());
    }
    let mut out = Value::Object(serde_json::Map::new());
    for path in paths {
        let steps: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
        if steps.is_empty() {
            return Err(NoSuchPath {
                path: path.clone(),
                kind: kind.to_string(),
            });
        }
        let Some(found) = pick(whole, &steps) else {
            return Err(NoSuchPath {
                path: path.clone(),
                kind: kind.to_string(),
            });
        };
        graft(whole, &mut out, &steps, found);
    }
    Ok(out)
}

fn pick(document: &Value, path: &[&str]) -> Option<Value> {
    let mut at = document;
    for step in path {
        at = match at.get(step) {
            Some(found) => found,
            None => at.get(snake(step))?,
        };
    }
    Some(at.clone())
}

/// `memoryMib` as `memory_mib`. A step already in snake_case comes back
/// unchanged, so this is only ever tried second and never changes a hit.
fn snake(step: &str) -> String {
    let mut out = String::with_capacity(step.len() + 4);
    for c in step.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Put the leaf back under the *document's* own spelling, never the caller's.
///
/// The result of a mask is fed straight into the same patch a REST client
/// writes, so a step spelled `memoryMib` has to land as `memory_mib` — keeping
/// the caller's spelling would produce a patch naming a field the model does
/// not have, and the change would be refused for a reason that is about the
/// mask rather than about the update.
fn graft(document: &Value, into: &mut Value, path: &[&str], leaf: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut source = document;
    let mut at = into;
    for step in parents {
        let key = key_in(source, step);
        source = source.get(&key).unwrap_or(&Value::Null);
        if !at.get(&key).map(Value::is_object).unwrap_or(false) {
            at[&key] = Value::Object(serde_json::Map::new());
        }
        at = &mut at[key];
    }
    at[key_in(source, last)] = leaf;
}

/// The key this document actually uses for `step`, whichever way it is spelled.
fn key_in(document: &Value, step: &str) -> String {
    if document.get(step).is_some() {
        step.to_string()
    } else {
        snake(step)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn whole() -> Value {
        json!({
            "spec": {
                "vcpus": 4,
                "memory_mib": 2048,
                "user_data": "#cloud-config\nthe tenant's own",
                "ports": ["projects/p1/ports/a"],
            },
            "meta": { "labels": { "env": "prod" } },
        })
    }

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The whole point: a resize does not write the guest's cloud-init.
    #[test]
    fn only_what_the_mask_names_is_carried_through() {
        let masked = apply(&whole(), &paths(&["spec.vcpus"]), "instances").unwrap();
        assert_eq!(masked, json!({ "spec": { "vcpus": 4 } }));
    }

    #[test]
    fn several_paths_are_kept_together() {
        let masked = apply(
            &whole(),
            &paths(&["spec.vcpus", "spec.memory_mib", "meta.labels"]),
            "instances",
        )
        .unwrap();
        assert_eq!(
            masked,
            json!({
                "spec": { "vcpus": 4, "memory_mib": 2048 },
                "meta": { "labels": { "env": "prod" } },
            })
        );
    }

    /// A client whose JSON mapping is lowerCamelCase hands its user the camel
    /// spelling, and there is no reading under which that is a typo. It lands
    /// under the document's own spelling, because the result is fed into the
    /// same patch a REST client writes.
    #[test]
    fn the_camel_spelling_of_a_path_finds_the_same_field() {
        let masked = apply(&whole(), &paths(&["spec.memoryMib"]), "instances").unwrap();
        assert_eq!(masked, json!({ "spec": { "memory_mib": 2048 } }));
    }

    /// A parent path keeps the whole branch, which is what AIP-134 says and
    /// what somebody writing `spec` expects.
    #[test]
    fn naming_a_branch_keeps_the_branch() {
        let masked = apply(&whole(), &paths(&["spec"]), "instances").unwrap();
        assert_eq!(masked["spec"], whole()["spec"]);
        assert!(masked.get("meta").is_none(), "{masked}");
    }

    /// Empty is every field the message carries — the behaviour every client
    /// already has, so adding masks breaks nobody.
    #[test]
    fn an_empty_mask_is_the_old_behaviour() {
        assert_eq!(apply(&whole(), &[], "instances").unwrap(), whole());
        assert_eq!(
            apply(&whole(), &paths(&[EVERYTHING]), "instances").unwrap(),
            whole()
        );
    }

    /// A caller who names a field nobody has has made a change they think
    /// happened. Refused, with the path in the sentence.
    #[test]
    fn a_path_nobody_has_is_refused_by_name() {
        let refusal =
            apply(&whole(), &paths(&["spec.vcpu"]), "instances").expect_err("a typo was accepted");
        assert_eq!(refusal.path, "spec.vcpu");
        assert!(refusal.to_string().contains("spec.vcpu"), "{refusal}");
        assert!(refusal.to_string().contains("instances"), "{refusal}");
    }

    #[test]
    fn an_empty_path_is_not_a_path() {
        assert!(apply(&whole(), &paths(&[""]), "instances").is_err());
    }

    /// A field whose value happens to be zero is still a field. The mask is
    /// about what was *named*, never about what the value is — which is the
    /// whole reason this exists.
    #[test]
    fn a_zero_is_carried_through_like_any_other_value() {
        let sent = json!({ "spec": { "vcpus": 0, "memory_mib": 2048 } });
        let masked = apply(&sent, &paths(&["spec.vcpus"]), "instances").unwrap();
        assert_eq!(masked, json!({ "spec": { "vcpus": 0 } }));
    }
}
