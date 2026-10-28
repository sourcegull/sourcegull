//! Postgres as Sourcegull sees it: reachability, connections, long transactions, size and the
//! processes' connection pools.

use crate::{Dashboard, Group, Observable, Owner, Threshold, Unit};

pub fn dashboard() -> Dashboard {
    Dashboard {
        name: "postgres",
        title: "Sourcegull: postgres",
        description: "Sourcegull's database, measured by the workers (`pg_stat_activity`, database size) and by every process (its connection pool). For server-level metrics (replication, I/O, vacuum), run a Postgres exporter next to it.",
        groups: vec![
            Group::new(
                "Database",
                vec![
                    vec![
                        Observable::new(
                            "postgres_up",
                            "workers reaching Postgres",
                            "max(sourcegull_postgres_up)",
                            Owner::Platform,
                        )
                        .critical(Threshold::below(1.0).for_minutes(5))
                        .interpretation("1 when the last metrics collection by a worker (every 30 s) reached Postgres, else 0.")
                        .next_steps(&[
                            "Check that Postgres runs and accepts connections (`pg_isready`), and its logs (disk full, too many connections, crash recovery).",
                            "Check `SOURCEGULL_DATABASE_URL` and the network path (Kubernetes NetworkPolicy, external database firewall).",
                            "All roles need Postgres: expect web, worker and repo alerts until it is back.",
                        ]),
                        Observable::new(
                            "connections",
                            "connections to Sourcegull's database by state",
                            "max by (state) (sourcegull_postgres_connections)",
                            Owner::Platform,
                        )
                        .legend("{{state}}")
                        .interpretation("From `pg_stat_activity`. Many `idle in transaction` connections point to transactions left open by a client."),
                        Observable::new(
                            "connection_saturation",
                            "share of max_connections in use by Sourcegull's database",
                            "sum(max by (state) (sourcegull_postgres_connections))\n/ max(sourcegull_postgres_max_connections)",
                            Owner::Platform,
                        )
                        .unit(Unit::Ratio)
                        .warning(Threshold::above(0.8).for_minutes(5))
                        .critical(Threshold::above(0.95).for_minutes(5))
                        .next_steps(&[
                            "Each Sourcegull process opens a pool per role; scaling out web or worker replicas multiplies connections. Compare the pools panel below with `max_connections`.",
                            "Raise `max_connections` in Postgres (and its memory accordingly), or put PgBouncer in transaction mode in front of it.",
                            "Lower the pool sizes (`SOURCEGULL_DB_MAX_CONNECTIONS`, `SOURCEGULL_DB_WORKER_MAX_CONNECTIONS`) of roles that hold many idle connections.",
                        ]),
                    ],
                    vec![
                        Observable::new(
                            "oldest_transaction",
                            "age of the oldest open transaction",
                            "max(sourcegull_postgres_oldest_transaction_seconds)",
                            Owner::Platform,
                        )
                        .unit(Unit::Seconds)
                        .warning(Threshold::above(1800.0).for_minutes(5))
                        .next_steps(&[
                            "Find it: `SELECT pid, usename, application_name, state, xact_start, query FROM pg_stat_activity WHERE xact_start IS NOT NULL ORDER BY xact_start LIMIT 5;`. Sourcegull's connections name their role in `application_name`.",
                            "Long transactions block vacuum and can hold locks other queries wait on. End it with `SELECT pg_terminate_backend(<pid>);` if it is stuck.",
                            "Schema migrations run in a transaction and can legitimately take long during an upgrade.",
                        ]),
                        Observable::new(
                            "database_size",
                            "size of Sourcegull's database",
                            "max(sourcegull_postgres_database_size_bytes)",
                            Owner::Platform,
                        )
                        .unit(Unit::Bytes)
                        .interpretation("Grows with repositories, code graph data and job history. Watch the free space of the Postgres volume."),
                    ],
                ],
            ),
            Group::new(
                "Connection pools",
                vec![vec![
                    Observable::new(
                        "pool_in_use",
                        "pool connections in use by process",
                        r#"max by (role, instance, pool) (sourcegull_db_pool_connections{state="in_use"})"#,
                        Owner::Platform,
                    )
                    .legend("{{role}} {{instance}} {{pool}}")
                    .interpretation("Connections each process has checked out of its pool. Workers next to other roles get a separate `worker` pool."),
                    Observable::new(
                        "pool_saturation",
                        "share of the connection pool in use by process",
                        r#"max by (role, instance, pool) (sourcegull_db_pool_connections{state="in_use"} / ignoring(state) sourcegull_db_pool_connections{state="max"})"#,
                        Owner::Platform,
                    )
                    .legend("{{role}} {{instance}} {{pool}}")
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.9).for_minutes(15))
                    .next_steps(&[
                        "Requests wait for a connection when the pool is exhausted: raise the pool size (`SOURCEGULL_DB_MAX_CONNECTIONS`, or `SOURCEGULL_DB_WORKER_MAX_CONNECTIONS` for the `worker` pool) if Postgres has spare connections (see connection saturation).",
                        "A pool that stays full while traffic is normal points to slow queries or long transactions: check the oldest transaction panel and the slow request logs.",
                    ]),
                ]],
            ),
        ],
    }
}
