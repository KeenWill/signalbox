use std::error::Error;

use rust_decimal::Decimal;

use super::migrated_postgres;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ownership_module_role_is_confined_to_its_schema() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let role: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT rolcanlogin, rolinherit, rolsuper, rolcreatedb, rolcreaterole, rolreplication
           FROM pg_roles
          WHERE rolname = 'mod_repo_watch'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(role, (false, false, false, false, false, false));

    let privileges: (bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT pg_has_role(current_user, 'mod_repo_watch', 'MEMBER'),
                has_schema_privilege('mod_repo_watch', 'mod_repo_watch', 'USAGE'),
                has_schema_privilege('mod_repo_watch', 'mod_repo_watch', 'CREATE'),
                has_table_privilege('mod_repo_watch', 'public.session', 'SELECT'),
                has_table_privilege('mod_repo_watch', 'public.session', 'REFERENCES')",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(privileges, (true, true, true, false, false));

    let public_table_grants: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM information_schema.role_table_grants
          WHERE grantee = 'mod_repo_watch'
            AND table_schema = 'public'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(public_table_grants, 0);

    let module_tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name
           FROM information_schema.tables
          WHERE table_schema = 'mod_repo_watch'
          ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        module_tables,
        [
            "core_event_cursor",
            "dispatch_ledger",
            "frontier",
            "gh_event",
            "pr_state",
            "repository_state",
            "rule",
            "rule_field_fingerprint",
            "rule_revision",
            "webhook_body",
            "webhook_delivery",
            "webhook_disposition",
        ]
    );

    let mut connection = pool.acquire().await?;
    sqlx::query("SET ROLE mod_repo_watch")
        .execute(&mut *connection)
        .await?;
    sqlx::query("SET search_path = mod_repo_watch, pg_catalog")
        .execute(&mut *connection)
        .await?;
    let cursor: Decimal =
        sqlx::query_scalar("SELECT applied_through FROM core_event_cursor WHERE singleton")
            .fetch_one(&mut *connection)
            .await?;
    assert_eq!(cursor, Decimal::ZERO);

    drop(pool);
    drop(container);
    Ok(())
}
