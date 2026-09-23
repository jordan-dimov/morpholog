//! Serde glue for a transition actor.
//!
//! An actor is always a [`Subject`], but it is stored and rendered as a tagged
//! [`EvalValue::Subject`] (`{"type":"subject","value":"..."}`), the same shape
//! subjects have in the `arguments` array. Deserialising checks the tag, so a
//! non-subject actor cannot get into the kernel.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{EvalValue, Subject};

pub fn serialize<S: Serializer>(actor: &Subject, serializer: S) -> Result<S::Ok, S::Error> {
    EvalValue::Subject(actor.clone()).serialize(serializer)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Subject, D::Error> {
    match EvalValue::deserialize(deserializer)? {
        EvalValue::Subject(s) => Ok(s),
        other => Err(serde::de::Error::custom(format!(
            "actor must be a subject, got {other:?}"
        ))),
    }
}
