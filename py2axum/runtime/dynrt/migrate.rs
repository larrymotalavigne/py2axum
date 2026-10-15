//! `<binary> migrate`: the project's alembic migrations applied without Python (py2axum/migrations.py renders
//! them at translation time with alembic's offline mode). `migrate [upgrade] [head|<revision>]` applies them
//! from the revision the database is at, in alembic's transactions (one for the whole run, or one per revision
//! with `transaction_per_migration`), logging like alembic through alembic.ini's logging configuration;
//! `migrate current` prints the database's revision; `migrate --sql` prints `alembic upgrade head --sql`.
//! The database is DATABASE_URL, like the server's.

use sqlx::{Connection, Executor, Row};

use super::v::*;

pub struct Step {
    pub revision: &'static str,
    /// what alembic logs: `upgrade a -> b, message`
    pub log: &'static str,
    /// the version table's rows once the revision is applied
    pub heads_after: &'static [&'static str],
    pub stmts: &'static [&'static str],
    /// `file:line: why` when the revision cannot be rendered offline
    pub refused: Option<&'static str>,
}

pub struct Plan {
    pub ini: &'static str,
    pub version_table: &'static str,
    pub version_schema: Option<&'static str>,
    pub per_migration: bool,
    pub create_version: &'static str,
    pub steps: &'static [Step],
    pub sql: &'static str,
    /// alembic.ini's logging sections as a dictConfig (JSON)
    pub logging: Option<&'static str>,
}

const LOGGER: &str = "alembic.runtime.migration";

fn info(msg: &str, args: Vec<V>) {
    super::logging::emit_sync(LOGGER, super::logging::INFO, msg, args, vec![], None, &super::logging::NO_LOC);
}

fn fail(msg: impl AsRef<str>) -> i32 {
    eprintln!("py2axum migrate: {}", msg.as_ref());
    1
}

/// the command line after `migrate`; the exit status
pub async fn run(plan: Option<&'static Plan>, args: Vec<String>) -> i32 {
    let Some(plan) = plan else {
        return fail("this binary has no migrations (no alembic.ini was found at the package or above when it was generated)");
    };
    let mut it = args.iter().map(String::as_str);
    let (cmd, target) = match (it.next(), it.next(), it.next()) {
        (None, ..) => ("upgrade", "head"),
        (Some("upgrade"), None, _) => ("upgrade", "head"),
        (Some("upgrade"), Some(t), None) => ("upgrade", t),
        (Some("current"), None, _) => ("current", ""),
        (Some("--sql"), None, _) => {
            print!("{}", plan.sql);
            return 0;
        }
        _ => return fail("usage: migrate [upgrade [head|<revision>]] | migrate current | migrate --sql"),
    };
    if let Some(cfg) = plan.logging {
        match super::pyd::loads(cfg).and_then(|c| super::logging::dict_config(&c)) {
            Ok(_) => {}
            Err(e) => return fail(format!("logging configuration of {}: {}", plan.ini, e.message())),
        }
    }
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgresql://postgres@127.0.0.1/postgres".into())
        .replace("postgresql+psycopg://", "postgresql://").replace("postgresql+asyncpg://", "postgresql://")
        .replace("postgresql+psycopg2://", "postgresql://");
    let mut conn = match sqlx::PgConnection::connect(&url).await {
        Ok(c) => c,
        Err(e) => return fail(format!("cannot connect to DATABASE_URL: {e}")),
    };
    // MigrationContext's first lines
    info("Context impl %s.", vec![V::str("PostgresqlImpl")]);
    info("Will assume %s DDL.", vec![V::str("transactional")]);
    let table = match plan.version_schema {
        Some(s) => format!("{s}.{}", plan.version_table),
        None => plan.version_table.to_string(),
    };
    let exists = match sqlx::query("SELECT to_regclass($1) IS NOT NULL").bind(&table).fetch_one(&mut conn).await {
        Ok(r) => r.get::<bool, _>(0),
        Err(e) => return fail(e.to_string()),
    };
    let mut heads: Vec<String> = vec![];
    if exists {
        match conn.fetch_all(format!("SELECT version_num FROM {table}").as_str()).await {
            Ok(rows) => heads = rows.iter().map(|r| r.get::<String, _>(0)).collect(),
            Err(e) => return fail(e.to_string()),
        }
        heads.sort();
    }
    if cmd == "current" {
        let last = plan.steps.last().map(|s| s.heads_after).unwrap_or(&[]);
        for h in &heads {
            println!("{h}{}", if last.contains(&h.as_str()) { " (head)" } else { "" });
        }
        return 0;
    }
    // where the database is on the upgrade path the binary was built with
    let start = if heads.is_empty() {
        0
    } else {
        match plan.steps.iter().position(|s| s.heads_after.iter().map(|x| x.to_string()).collect::<Vec<_>>() == heads) {
            Some(i) => i + 1,
            None => return fail(format!("the database is at {:?}, which is not on the upgrade path of {}", heads, plan.ini)),
        }
    };
    let end = if target == "head" || target == "heads" {
        plan.steps.len()
    } else {
        match plan.steps.iter().position(|s| s.revision == target) {
            Some(i) if i + 1 >= start => i + 1,
            Some(_) => return fail(format!("revision {target} is already applied (a downgrade is not supported)")),
            None => return fail(format!("no revision {target:?} in {}", plan.ini)),
        }
    };
    let todo = &plan.steps[start.min(end)..end];
    if let Some(s) = todo.iter().find(|s| s.refused.is_some()) {
        return fail(format!("revision {} cannot be applied by the binary: {} (apply it with alembic)", s.revision, s.refused.unwrap()));
    }
    let result: Result<(), sqlx::Error> = async {
        if !plan.per_migration {
            conn.execute("BEGIN").await?;
        }
        if !exists && !plan.create_version.is_empty() {
            conn.execute(plan.create_version).await?;
        }
        for s in todo {
            if plan.per_migration {
                conn.execute("BEGIN").await?;
            }
            info("Running %s", vec![V::str(s.log)]);
            for stmt in s.stmts {
                conn.execute(*stmt).await?;
            }
            if plan.per_migration {
                conn.execute("COMMIT").await?;
            }
        }
        if !plan.per_migration {
            conn.execute("COMMIT").await?;
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = conn.execute("ROLLBACK").await;
            fail(e.to_string())
        }
    }
}
