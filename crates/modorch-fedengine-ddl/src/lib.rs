//! Fail-closed SQL analysis, source binding and controlled index templates.
mod analysis;
mod binding;
mod ddl_intent;
mod index;
mod model;
mod mutation;
mod parameters;
mod pushdown;

pub use analysis::{Analyzer, BindingNamespace, RelationNames};
pub use ddl_intent::is_lossless_type_widening;
pub use index::{drop_index_name, index_ownership_target};
pub use model::{Analysis, DdlIntent, IndexIntent, IndexOwner, Kind, ObjectRef};
pub use mutation::{build_guarded_mutation_plan, validate_kingbase_text_literals};
pub use parameters::{cast_parameters, compact_parameters, parameter_hints};
pub use pushdown::{
    rewrite as rewrite_pushdown, rewrite_with_timezone as rewrite_pushdown_with_timezone,
};
