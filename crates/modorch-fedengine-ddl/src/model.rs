use modorch_fedengine_contract::{DdlColumnSpec, DdlConstraintSpec, GuardedDdlPlan, ReturningPlan};
use modorch_fedengine_core::{DataSourceDefinition, DataSourceId, Result, quote_identifier};
use std::collections::BTreeSet;
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectRef {
    pub source: DataSourceId,
    pub catalog: String,
    pub schema: String,
    pub name: String,
}
impl ObjectRef {
    pub fn sql(&self) -> String {
        [
            self.catalog.as_str(),
            self.schema.as_str(),
            self.name.as_str(),
        ]
        .map(quote_identifier)
        .join(".")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Query,
    Insert,
    Update,
    Delete,
    Ddl,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlIntent {
    pub action: String,
    pub table_name: String,
    pub index_name: String,
    pub owner: String,
    pub new_name: String,
    pub column_name: String,
    pub default_kind: String,
    pub default_value: String,
    pub constraint_name: String,
    pub constraint: Option<DdlConstraintSpec>,
    pub constraints: Vec<DdlConstraintSpec>,
    pub identity_mode: String,
    pub columns: Vec<DdlColumnSpec>,
    pub keys: Vec<String>,
    pub conditional: bool,
    pub only: bool,
    pub query_sql: Option<String>,
}
impl DdlIntent {
    /// Convert an analyzed DDL intent into a typed native guarded-DDL plan for `source`.
    pub fn into_guarded_plan(
        self,
        source: &DataSourceDefinition,
        database: String,
        schema: String,
        owner: String,
    ) -> Result<Option<GuardedDdlPlan>> {
        crate::ddl_intent::build_guarded_plan(self, source, database, schema, owner)
    }
}
#[derive(Debug, Clone)]
pub struct Analysis {
    pub sql: String,
    pub kind: Kind,
    pub action: String,
    pub capability: String,
    pub command: String,
    pub objects: BTreeSet<ObjectRef>,
    pub sources: BTreeSet<DataSourceId>,
    pub primary: Option<ObjectRef>,
    pub index: Option<IndexIntent>,
    pub returning: Option<ReturningPlan>,
    pub ddl_intent: Option<DdlIntent>,
}
#[derive(Debug, Clone)]
pub struct IndexOwner {
    pub index: ObjectRef,
    pub table: ObjectRef,
    pub protected: bool,
}
#[derive(Debug, Clone)]
pub struct IndexIntent {
    pub table: ObjectRef,
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub create: bool,
    pub if_exists: bool,
}
