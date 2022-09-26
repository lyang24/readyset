use launchpad::hash::hash;
use mysql_async::prelude::*;
use readyset_client::backend::UnsupportedSetMode;
use readyset_client::query_status_cache::hash_to_query_id;
use readyset_client::BackendBuilder;
use readyset_client_metrics::QueryDestination;
use readyset_client_test_helpers::mysql_helpers::{last_query_info, MySQLAdapter};
use readyset_client_test_helpers::{self, sleep, TestBuilder};
use readyset_server::Handle;

async fn setup_with(backend_builder: BackendBuilder) -> (mysql_async::Opts, Handle) {
    TestBuilder::new(backend_builder)
        .fallback(true)
        .build::<MySQLAdapter>()
        .await
}

async fn setup() -> (mysql_async::Opts, Handle) {
    setup_with(BackendBuilder::new().require_authentication(false)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn create_table() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE Cats (id int, PRIMARY KEY(id))")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("INSERT INTO Cats (id) VALUES (1)")
        .await
        .unwrap();
    sleep().await;

    let row: Option<(i32,)> = conn
        .query_first("SELECT Cats.id FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(row, Some((1,)))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore] // alter table not supported yet
async fn add_column() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE Cats (id int, PRIMARY KEY(id))")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("INSERT INTO Cats (id) VALUES (1)")
        .await
        .unwrap();
    sleep().await;

    let row: Option<(i32,)> = conn
        .query_first("SELECT Cats.id FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(row, Some((1,)));

    conn.query_drop("ALTER TABLE Cats ADD COLUMN name TEXT;")
        .await
        .unwrap();
    conn.query_drop("UPDATE Cats SET name = 'Whiskers' WHERE id = 1;")
        .await
        .unwrap();
    sleep().await;

    let row: Option<(i32, String)> = conn
        .query_first("SELECT Cats.id, Cats.name FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(row, Some((1, "Whiskers".to_owned())));
}

#[tokio::test(flavor = "multi_thread")]
async fn json_column_insert_read() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE Cats (id int PRIMARY KEY, data JSON)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("INSERT INTO Cats (id, data) VALUES (1, '{\"name\": \"Mr. Mistoffelees\"}')")
        .await
        .unwrap();
    sleep().await;
    sleep().await;

    let rows: Vec<(i32, String)> = conn
        .query("SELECT * FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(
        rows,
        vec![(1, "{\"name\":\"Mr. Mistoffelees\"}".to_string())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn json_column_partial_update() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE Cats (id int PRIMARY KEY, data JSON)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("INSERT INTO Cats (id, data) VALUES (1, '{\"name\": \"xyz\"}')")
        .await
        .unwrap();
    conn.query_drop("UPDATE Cats SET data = JSON_REMOVE(data, '$.name') WHERE id = 1")
        .await
        .unwrap();
    sleep().await;

    let rows: Vec<(i32, String)> = conn
        .query("SELECT * FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, "{}".to_string())]);
}

// TODO: remove this once we support range queries again
#[tokio::test(flavor = "multi_thread")]
async fn range_query() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE cats (id int PRIMARY KEY, cuteness int)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO cats (id, cuteness) values (1, 10)")
        .await
        .unwrap();
    sleep().await;

    let rows: Vec<(i32, i32)> = conn
        .exec("SELECT id, cuteness FROM cats WHERE cuteness > ?", vec![5])
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, 10)]);
}

// TODO: remove this once we support aggregates on parametrized IN
#[tokio::test(flavor = "multi_thread")]
async fn aggregate_in() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE cats (id int PRIMARY KEY, cuteness int)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO cats (id, cuteness) values (1, 10), (2, 8)")
        .await
        .unwrap();
    sleep().await;

    let rows: Vec<(i32,)> = conn
        .exec(
            "SELECT sum(cuteness) FROM cats WHERE id IN (?, ?)",
            vec![1, 2],
        )
        .await
        .unwrap();
    assert_eq!(rows, vec![(18,)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_autocommit() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    // We do not support SET autocommit = 0;
    assert!(conn
        .query_drop("SET @@SESSION.autocommit = 1;")
        .await
        .is_ok());
    assert!(conn
        .query_drop("SET @@SESSION.autocommit = 0;")
        .await
        .is_err());
    assert!(conn.query_drop("SET @@LOCAL.autocommit = 1;").await.is_ok());
    assert!(conn
        .query_drop("SET @@LOCAL.autocommit = 0;")
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_unsupported_sets() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();

    sleep().await;

    conn.query_drop("SET @@SESSION.SQL_MODE = 'ANSI_QUOTES';")
        .await
        .unwrap();

    // We should proxy the SET statement upstream, then all subsequent statements should go upstream
    // (evidenced by the fact that `"x"` is interpreted as a column reference, per the ANSI_QUOTES
    // SQL mode)
    assert_eq!(
        conn.query_first::<(i32,), _>("SELECT \"x\" FROM \"t\"")
            .await
            .unwrap()
            .unwrap()
            .0,
        1,
    );

    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_unsupported_sets_prep_exec() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();

    sleep().await;

    conn.query_drop("SET @@SESSION.SQL_MODE = 'ANSI_QUOTES';")
        .await
        .unwrap();

    conn.exec_drop("SELECT * FROM t", ()).await.unwrap();

    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prepare_in_tx_select_out() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();
    let prepared = tx.prep("SELECT * FROM t").await.unwrap();
    tx.commit().await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prep_and_select_in_tx() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();

    let prepared = tx.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = tx.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut tx).await.destination,
        QueryDestination::Upstream
    );
    tx.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn prep_then_select_in_tx() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();

    let _: Option<i64> = tx.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut tx).await.destination,
        QueryDestination::Upstream
    );
    tx.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn prep_then_always_select_in_tx() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;
    let prepared = conn.prep("SELECT x FROM t").await.unwrap();
    conn.query_drop("CREATE CACHE ALWAYS test_always FROM SELECT x FROM t;")
        .await
        .unwrap();
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();

    let _: Option<i64> = tx.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut tx).await.destination,
        QueryDestination::Readyset
    );
    tx.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn always_should_bypass_tx() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("CREATE CACHE ALWAYS test_always FROM SELECT x FROM t;")
        .await
        .unwrap();
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();

    tx.query_drop("SELECT x FROM t").await.unwrap();

    assert_eq!(
        last_query_info(&mut tx).await.destination,
        QueryDestination::Readyset
    );
    tx.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn prep_select() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();

    sleep().await;

    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_then_prep_and_select() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("set @foo = 5").await.unwrap();
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn always_should_never_proxy() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    conn.query_drop("CREATE CACHE ALWAYS FROM SELECT * FROM t")
        .await
        .unwrap();
    conn.query_drop("set @foo = 5").await.unwrap();
    conn.query_drop("SELECT * FROM t").await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn always_should_never_proxy_exec() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("CREATE CACHE ALWAYS FROM SELECT * FROM t")
        .await
        .unwrap();
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );
    conn.query_drop("set @foo = 5").await.unwrap();
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prep_then_set_then_select_proxy() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("set @foo = 5").await.unwrap();
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_mode_should_allow_commands() {
    let (opts, _handle) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("SET @@SESSION.SQL_MODE = 'ANSI_QUOTES';")
        .await
        .unwrap();

    // We should proxy the SET statement upstream, then all subsequent statements should go upstream
    // (evidenced by the fact that `"x"` is interpreted as a column reference, per the ANSI_QUOTES
    // SQL mode)
    assert_eq!(
        conn.query_first::<(i32,), _>("SELECT \"x\" FROM \"t\"")
            .await
            .unwrap()
            .unwrap()
            .0,
        1,
    );

    // We should still handle custom ReadySet commands directly, otherwise we will end up passing
    // back errors from the upstream database for queries it doesn't recognize.
    // This validates what we already just validated (that the query went to upstream) and also
    // that EXPLAIN LAST STATEMENT, a ReadySet command, was handled directly by ReadySet and not
    // proxied upstream.
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn drop_then_recreate_table_with_query() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    sleep().await;

    conn.query_drop("CREATE CACHE q FROM SELECT x FROM t")
        .await
        .unwrap();
    conn.query_drop("SELECT x FROM t").await.unwrap();
    conn.query_drop("DROP TABLE t").await.unwrap();

    sleep().await;

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("CREATE CACHE q FROM SELECT x FROM t")
        .await
        .unwrap();
    // Query twice to clear the cache
    conn.query_drop("SELECT x FROM t").await.unwrap();
    conn.query_drop("SELECT x FROM t").await.unwrap();

    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn transaction_proxies() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    sleep().await;

    conn.query_drop("CREATE CACHE FROM SELECT * FROM t")
        .await
        .unwrap();

    conn.query_drop("BEGIN;").await.unwrap();
    conn.query_drop("SELECT * FROM t;").await.unwrap();

    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );

    conn.query_drop("COMMIT;").await.unwrap();

    conn.query_drop("SELECT * FROM t;").await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn valid_sql_parsing_failed_shows_proxied() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    // This query needs to be valid SQL but fail to parse. The current query is one known to not be
    // supported by the parser, but if this test is failing, that may no longer be the case and
    // this the query should be replaced with another one that isn't supported, or a more
    // complicated way of testing this needs to be devised
    let q = "CREATE TABLE t1 (id polygon);".to_string();
    let _ = conn.query_drop(q.clone()).await;

    let proxied_queries = conn
        .query::<(String, String, String), _>("SHOW PROXIED QUERIES;")
        .await
        .unwrap();
    let id = hash_to_query_id(hash(&q));
    assert!(
        proxied_queries.contains(&(id, q.clone(), "unsupported".to_owned())),
        "proxied_queries = {:?}",
        proxied_queries,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_sql_parsing_failed_doesnt_show_proxied() {
    let (opts, _handle) = setup().await;
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();

    let q = "this isn't valid SQL".to_string();
    let _ = conn.query_drop(q.clone()).await;
    let proxied_queries = conn
        .query::<(String, String, String), _>("SHOW PROXIED QUERIES;")
        .await
        .unwrap();

    assert!(proxied_queries.is_empty());
}
