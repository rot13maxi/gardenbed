//! The compost framework: derived data computed recursively from parents.
//!
//! value(cs) = compute(cs, values(parents)). Each kind of derived data
//! chooses eager materialization (mainline) or lazy (elsewhere), and stays
//! invisible to clients so internals can change freely.

use gardenbed_model::Changeset;

pub trait DerivedData {
    type Value;

    fn compute(changeset: &Changeset, parent_values: &[Self::Value]) -> Self::Value;
}
