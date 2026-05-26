moved {
  from = module.postgresql.random_id.db_name_suffix
  to   = module.postgresql_instance.random_id.db_name_suffix
}

moved {
  from = module.postgresql.google_sql_database_instance.instance
  to   = module.postgresql_instance.google_sql_database_instance.instance
}

moved {
  from = module.postgresql.random_password.admin
  to   = module.postgresql_instance.random_password.admin
}

moved {
  from = module.postgresql.google_sql_user.admin
  to   = module.postgresql_instance.google_sql_user.admin
}

moved {
  from = module.postgresql.module.database
  to   = module.postgresql_database
}
