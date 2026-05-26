//! Serde glue for a transition actor.
//!
//! An actor is always a [`Subject`], but it persists and renders as a tagged
//! [`EvalValue::Subject`] so the audit `actor` column and the CLI's transition
//! JSON keep their v0 shape (`{"type":"subject","value":"..."}`), consistent
//! with how subjects appear in the `arguments` array. Deserialisation validates
//! the tag at the IO boundary - the one place a non-subject actor could enter -
//! so the kernel itself needs no runtime "actor must be a subject" check.

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
