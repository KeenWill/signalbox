use std::error::Error;

use rust_decimal::Decimal;
use signalbox_persistence::local_test_connection_options;
use sqlx::postgres::PgPoolOptions;

use super::migrated_postgres;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ownership_module_role_is_confined_to_its_schema() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;

    let role: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT rolcanlogin, rolinherit, rolsuper, rolcreatedb, rolcreaterole, rolreplication
           FROM pg_roles
          WHERE rolname = 'mod_repo_watch'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(role, (true, false, false, false, false, false));

    let privileges: (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT has_schema_privilege('mod_repo_watch', 'mod_repo_watch', 'USAGE'),
                has_schema_privilege('mod_repo_watch', 'mod_repo_watch', 'CREATE'),
                has_table_privilege('mod_repo_watch', 'public.session', 'SELECT'),
                has_table_privilege('mod_repo_watch', 'public.session', 'REFERENCES')",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(privileges, (true, true, false, false));

    let function_privileges: (bool, bool) = sqlx::query_as(
        "SELECT has_function_privilege('mod_repo_watch',
                    'public.configured_git_remote_url_is_valid(text)', 'EXECUTE'),
                has_function_privilege(current_user,
                    'public.configured_git_remote_url_is_valid(text)', 'EXECUTE')",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(function_privileges, (false, true));

    let core_membership: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM pg_auth_members membership
               JOIN pg_roles granted_role ON granted_role.oid = membership.roleid
               JOIN pg_roles member_role ON member_role.oid = membership.member
              WHERE granted_role.rolname = 'mod_repo_watch'
                AND member_role.rolname = current_user
         )",
    )
    .fetch_one(&pool)
    .await?;
    assert!(!core_membership);

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

    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&pool)
        .await?;
    let module_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(local_test_connection_options(&database_url)?.username("mod_repo_watch"))
        .await?;
    let mut connection = module_pool.acquire().await?;
    let identities: (String, String) =
        sqlx::query_as("SELECT session_user::text, current_user::text")
            .fetch_one(&mut *connection)
            .await?;
    assert_eq!(
        identities,
        ("mod_repo_watch".into(), "mod_repo_watch".into())
    );

    sqlx::query("RESET ROLE").execute(&mut *connection).await?;
    let reset_identity: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *connection)
        .await?;
    assert_eq!(reset_identity, "mod_repo_watch");

    sqlx::query("SET search_path = mod_repo_watch, pg_catalog")
        .execute(&mut *connection)
        .await?;
    let cursor: Decimal =
        sqlx::query_scalar("SELECT applied_through FROM core_event_cursor WHERE singleton")
            .fetch_one(&mut *connection)
            .await?;
    assert_eq!(cursor, Decimal::ZERO);

    let effective_function_privilege: bool = sqlx::query_scalar(
        "SELECT has_function_privilege(
            current_user, 'public.configured_git_remote_url_is_valid(text)', 'EXECUTE')",
    )
    .fetch_one(&mut *connection)
    .await?;
    assert!(!effective_function_privilege);

    let core_read = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM public.session")
        .fetch_one(&mut *connection)
        .await;
    assert_eq!(
        core_read
            .expect_err("the module login cannot read a core table")
            .as_database_error()
            .and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("42501")),
    );

    drop(connection);
    drop(module_pool);
    drop(pool);
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn v1_retirement_preserves_commissioned_dispatch_validation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname
           FROM pg_constraint
          WHERE conrelid = 'commissioned_dispatch'::regclass
            AND conname IN (
                'commissioned_dispatch_base_branch_check',
                'commissioned_dispatch_branch_check',
                'commissioned_dispatch_head_branch_check',
                'commissioned_dispatch_head_repository_check',
                'commissioned_dispatch_repository_check'
            )
          ORDER BY conname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        constraints,
        [
            "commissioned_dispatch_base_branch_check",
            "commissioned_dispatch_branch_check",
            "commissioned_dispatch_head_branch_check",
            "commissioned_dispatch_head_repository_check",
            "commissioned_dispatch_repository_check",
        ]
    );

    let validation: (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT repo_watch_repository_is_valid('owner/repository'),
                repo_watch_repository_is_valid('invalid'),
                repo_watch_branch_is_valid('main'),
                repo_watch_branch_is_valid('')",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(validation, (true, false, true, false));

    drop(pool);
    drop(container);
    Ok(())
}
