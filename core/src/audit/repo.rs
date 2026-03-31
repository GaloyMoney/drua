use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "AuditEntry",
    columns(
        subject(
            ty = "String",
            create(accessor = "subject_string()"),
            list_for(by(created_at)),
            update(persist = false)
        ),
        interaction_type(
            ty = "String",
            create(accessor = "interaction_type_string()"),
            update(persist = false)
        ),
        action(ty = "String", update(persist = false))
    )
)]
pub struct AuditEntryRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl AuditEntryRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
