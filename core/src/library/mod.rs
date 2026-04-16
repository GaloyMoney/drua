use sqlx::PgPool;

#[derive(Clone)]
pub struct Library {
    #[allow(dead_code)]
    pool: PgPool,
}

impl Library {
    pub fn new(pool: &PgPool, _jobs: &mut job::Jobs) -> Self {
        Self { pool: pool.clone() }
    }
}
