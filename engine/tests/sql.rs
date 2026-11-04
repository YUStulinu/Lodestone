//! SQL end to end, on the simulated disk.

use std::path::Path;

use lodestone::Session;
use lodestone::sql::{Database, Output, Value};
use lodestone::storage::Options;
use lodestone::storage::vfs::SimVfs;

fn db() -> Database {
    Database::open_with(&SimVfs::new(), Path::new("t.db"), Options::default()).unwrap()
}

/// Rows of the last statement, each value as text ("NULL" for null).
fn q(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    let out = s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match out.into_iter().last().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| match v {
                        Value::Null => "NULL".into(),
                        Value::Text(t) => t,
                        v => v.to_string(),
                    })
                    .collect()
            })
            .collect(),
        Output::Command(c) => vec![vec![c]],
    }
}

fn flat(rows: Vec<Vec<String>>) -> String {
    rows.into_iter().map(|r| r.join(",")).collect::<Vec<_>>().join(" | ")
}

fn setup(s: &mut Session) {
    s.execute(
        "CREATE TABLE dept (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, dept INTEGER, salary REAL, boss INTEGER);
         INSERT INTO dept VALUES (1, 'eng'), (2, 'ops'), (3, 'empty');
         INSERT INTO emp VALUES (1, 'ada', 1, 100, NULL), (2, 'bob', 1, 80, 1), (3, 'cy', 2, 70, 1),
                                (4, 'di', 2, 75, 3), (5, 'ed', NULL, 50, 3);",
    )
    .unwrap();
}

#[test]
fn queries() {
    let db = db();
    let mut s = db.session();
    setup(&mut s);
    assert_eq!(flat(q(&mut s, "SELECT name FROM emp WHERE salary > 72 ORDER BY salary DESC")), "ada | bob | di");
    assert_eq!(flat(q(&mut s, "SELECT 1 + 2 * 3, 7 / 2, 7.0 / 2, 'a' || 'b', -5 % 3")), "7,3,3.5,ab,-2");
    // Join, both written ways; the optimiser's reordering must not change the columns.
    let a = q(&mut s, "SELECT e.name, d.name FROM emp e, dept d WHERE e.dept = d.id ORDER BY e.id");
    let b = q(&mut s, "SELECT e.name, d.name FROM emp e JOIN dept d ON e.dept = d.id ORDER BY e.id");
    assert_eq!(a, b);
    assert_eq!(flat(a), "ada,eng | bob,eng | cy,ops | di,ops");
    assert_eq!(
        flat(q(&mut s, "SELECT d.name, e.name FROM dept d LEFT JOIN emp e ON e.dept = d.id AND e.salary > 72 ORDER BY d.id, e.id")),
        "eng,ada | eng,bob | ops,di | empty,NULL"
    );
    assert_eq!(flat(q(&mut s, "SELECT e.name, d.name FROM emp e RIGHT JOIN dept d ON e.dept = d.id WHERE d.id = 3")), "NULL,empty");
    // Aggregates.
    assert_eq!(
        flat(q(
            &mut s,
            "SELECT dept, count(*), sum(salary), avg(salary), min(name), max(salary) FROM emp GROUP BY dept ORDER BY dept NULLS FIRST"
        )),
        "NULL,1,50,50,ed,50 | 1,2,180,90,ada,100 | 2,2,145,72.5,cy,75"
    );
    assert_eq!(flat(q(&mut s, "SELECT count(*), count(dept), count(DISTINCT dept) FROM emp")), "5,4,2");
    assert_eq!(flat(q(&mut s, "SELECT count(*), sum(salary) FROM emp WHERE id > 99")), "0,NULL");
    assert_eq!(flat(q(&mut s, "SELECT dept FROM emp GROUP BY dept HAVING count(*) > 1 ORDER BY 1")), "1 | 2");
    assert!(s.execute("SELECT name, count(*) FROM emp").is_err());
    // Subqueries: uncorrelated, correlated, EXISTS, IN, scalar, and correlated over GROUP BY.
    assert_eq!(flat(q(&mut s, "SELECT name FROM emp WHERE salary > (SELECT avg(salary) FROM emp) ORDER BY 1")), "ada | bob"); // avg is 75
    assert_eq!(
        flat(q(&mut s, "SELECT name FROM emp e WHERE salary > (SELECT avg(salary) FROM emp x WHERE x.dept = e.dept) ORDER BY 1")),
        "ada | di"
    );
    assert_eq!(flat(q(&mut s, "SELECT name FROM dept d WHERE NOT EXISTS (SELECT 1 FROM emp WHERE dept = d.id)")), "empty");
    assert_eq!(flat(q(&mut s, "SELECT name FROM emp WHERE boss IN (SELECT id FROM emp WHERE dept = 2) ORDER BY 1")), "di | ed");
    assert_eq!(flat(q(&mut s, "SELECT name FROM emp WHERE id NOT IN (SELECT boss FROM emp) ORDER BY 1")), "");
    assert_eq!(
        flat(q(&mut s, "SELECT name, (SELECT name FROM emp b WHERE b.id = e.boss) FROM emp e ORDER BY id")),
        "ada,NULL | bob,ada | cy,ada | di,cy | ed,cy"
    );
    assert_eq!(
        flat(q(&mut s, "SELECT dept, (SELECT name FROM dept WHERE id = e.dept) FROM emp e GROUP BY dept ORDER BY 1")),
        "1,eng | 2,ops | NULL,NULL"
    );
    // Set operations, DISTINCT, LIMIT/OFFSET, CASE, IN lists, BETWEEN, LIKE, NULL logic.
    assert_eq!(flat(q(&mut s, "SELECT dept FROM emp UNION SELECT id FROM dept ORDER BY 1")), "1 | 2 | 3 | NULL");
    assert_eq!(flat(q(&mut s, "SELECT id FROM dept EXCEPT SELECT dept FROM emp")), "3");
    assert_eq!(flat(q(&mut s, "SELECT DISTINCT dept FROM emp WHERE dept IS NOT NULL ORDER BY dept DESC")), "2 | 1");
    assert_eq!(flat(q(&mut s, "SELECT id FROM emp ORDER BY id LIMIT 2 OFFSET 1")), "2 | 3");
    assert_eq!(
        flat(q(&mut s, "SELECT CASE WHEN salary >= 80 THEN 'hi' WHEN salary >= 70 THEN 'mid' END FROM emp ORDER BY id")),
        "hi | hi | mid | mid | NULL"
    );
    assert_eq!(flat(q(&mut s, "SELECT name FROM emp WHERE id IN (2, 4) OR name LIKE 'e%' ORDER BY 1")), "bob | di | ed");
    assert_eq!(flat(q(&mut s, "SELECT count(*) FROM emp WHERE salary BETWEEN 70 AND 80")), "3");
    assert_eq!(
        flat(q(&mut s, "SELECT NULL = NULL, NULL IS NULL, 1 IN (2, NULL), NOT (NULL AND FALSE), coalesce(NULL, 3)")),
        "NULL,TRUE,NULL,TRUE,3"
    );
    // Errors carry PostgreSQL codes.
    assert_eq!(s.execute("SELECT 1/0").unwrap_err().code, "22012");
    assert_eq!(s.execute("SELECT nope FROM emp").unwrap_err().code, "42703");
    assert_eq!(s.execute("SELECT * FROM nope").unwrap_err().code, "42P01");
    assert_eq!(s.execute("SELECT (SELECT id FROM emp)").unwrap_err().code, "21000");
    assert_eq!(s.execute("SELEC 1").unwrap_err().code, "42601");
}

#[test]
fn writes_indexes_and_constraints() {
    let db = db();
    let mut s = db.session();
    setup(&mut s);
    assert_eq!(s.execute("INSERT INTO emp VALUES (1, 'dup', 1, 1, NULL)").unwrap_err().code, "23505");
    assert_eq!(s.execute("INSERT INTO dept (id) VALUES (9)").unwrap_err().code, "23502");
    assert_eq!(flat(q(&mut s, "UPDATE emp SET salary = salary * 1.1 WHERE dept = 1")), "UPDATE 2");
    assert_eq!(flat(q(&mut s, "SELECT round(salary, 1) FROM emp WHERE id = 2")), "88");
    s.execute("CREATE INDEX emp_dept ON emp (dept); CREATE UNIQUE INDEX emp_name ON emp (name)").unwrap();
    assert_eq!(s.execute("INSERT INTO emp VALUES (9, 'ada', 1, 1, NULL)").unwrap_err().code, "23505");
    // The index is used, and gives the same answers.
    let plan = flat(q(&mut s, "EXPLAIN SELECT name FROM emp WHERE dept = 2"));
    assert!(plan.contains("Index Scan using emp_dept"), "{plan}");
    assert_eq!(flat(q(&mut s, "SELECT name FROM emp WHERE dept = 2 ORDER BY 1")), "cy | di");
    assert!(flat(q(&mut s, "EXPLAIN SELECT name FROM emp WHERE id = 3")).contains("Primary Key Lookup"));
    assert!(flat(q(&mut s, "EXPLAIN SELECT name FROM emp WHERE id > 3")).contains("Primary Key Range Scan"));
    assert_eq!(flat(q(&mut s, "SELECT name FROM emp WHERE id > 3 ORDER BY id")), "di | ed");
    assert_eq!(flat(q(&mut s, "SELECT name FROM emp WHERE id >= 2.5 AND id < 4.5 ORDER BY id")), "cy | di");
    // Moving a row's key and an indexed value keeps the indexes right.
    s.execute("UPDATE emp SET id = 40, dept = 1, name = 'dee' WHERE id = 4").unwrap();
    assert_eq!(flat(q(&mut s, "SELECT id FROM emp WHERE dept = 1 ORDER BY id")), "1 | 2 | 40");
    assert_eq!(flat(q(&mut s, "SELECT id FROM emp WHERE name = 'dee'")), "40");
    assert_eq!(flat(q(&mut s, "SELECT count(*) FROM emp WHERE name = 'di'")), "0");
    assert_eq!(flat(q(&mut s, "DELETE FROM emp WHERE dept = 1")), "DELETE 3");
    assert_eq!(flat(q(&mut s, "SELECT count(*) FROM emp WHERE dept = 1")), "0");
    // Tables without a primary key, INSERT ... SELECT from itself, defaults, views.
    s.execute("CREATE TABLE log (msg TEXT DEFAULT 'none', n INTEGER); INSERT INTO log (n) VALUES (1), (2)").unwrap();
    s.execute("INSERT INTO log SELECT msg, n + 10 FROM log").unwrap();
    assert_eq!(flat(q(&mut s, "SELECT msg, n FROM log ORDER BY n")), "none,1 | none,2 | none,11 | none,12");
    s.execute("CREATE VIEW big AS SELECT n FROM log WHERE n > 10").unwrap();
    assert_eq!(flat(q(&mut s, "SELECT sum(n) FROM big")), "23");
    s.execute("DROP VIEW big; DROP TABLE log").unwrap();
    assert_eq!(s.execute("SELECT * FROM log").unwrap_err().code, "42P01");
}

#[test]
fn transactions_and_conflicts() {
    let db = db();
    let mut a = db.session();
    let mut b = db.session();
    a.execute("CREATE TABLE acct (id INTEGER PRIMARY KEY, balance INTEGER)").unwrap();
    a.execute("INSERT INTO acct VALUES (1, 100), (2, 100)").unwrap();
    a.execute("BEGIN").unwrap();
    a.execute("UPDATE acct SET balance = balance - 30 WHERE id = 1").unwrap();
    // b does not see a's uncommitted change.
    assert_eq!(flat(q(&mut b, "SELECT balance FROM acct WHERE id = 1")), "100");
    // b writing the same row conflicts, and its transaction is then aborted.
    b.execute("BEGIN").unwrap();
    assert_eq!(b.execute("UPDATE acct SET balance = 0 WHERE id = 1").unwrap_err().code, "40001");
    assert_eq!(b.execute("SELECT 1").unwrap_err().code, "25P02");
    assert_eq!(flat(q(&mut b, "COMMIT")), "ROLLBACK");
    a.execute("UPDATE acct SET balance = balance + 30 WHERE id = 2; COMMIT").unwrap();
    assert_eq!(flat(q(&mut b, "SELECT sum(balance), min(balance) FROM acct")), "200,70");
    // Rollback undoes, DDL included.
    a.execute("BEGIN; INSERT INTO acct VALUES (3, 5); CREATE TABLE tmp (x INTEGER); ROLLBACK").unwrap();
    assert_eq!(flat(q(&mut a, "SELECT count(*) FROM acct")), "2");
    assert_eq!(a.execute("SELECT * FROM tmp").unwrap_err().code, "42P01");
    // A snapshot stays fixed for the whole transaction.
    b.execute("BEGIN").unwrap();
    assert_eq!(flat(q(&mut b, "SELECT count(*) FROM acct")), "2");
    a.execute("INSERT INTO acct VALUES (4, 1)").unwrap();
    assert_eq!(flat(q(&mut b, "SELECT count(*) FROM acct")), "2");
    b.execute("COMMIT").unwrap();
    assert_eq!(flat(q(&mut b, "SELECT count(*) FROM acct")), "3");
    assert!(db.mvcc().vacuum().unwrap() > 0);
    assert_eq!(flat(q(&mut b, "SELECT sum(balance) FROM acct")), "201");
}

#[test]
fn explain_analyze_and_parameters() {
    let db = db();
    let mut s = db.session();
    setup(&mut s);
    let text = flat(q(&mut s, "EXPLAIN ANALYZE SELECT d.name, count(*) FROM emp e JOIN dept d ON e.dept = d.id GROUP BY d.name"));
    assert!(text.contains("Hash Join") && text.contains("actual time=") && text.contains("Execution Time"), "{text}");
    let json = flat(q(&mut s, "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM emp WHERE salary > 60"));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0]["Plan"]["Node Type"], "Seq Scan on emp");
    assert_eq!(v[0]["Plan"]["Actual Rows"], 4);
    let out = s
        .execute_params("SELECT name FROM emp WHERE dept = $1 AND salary > $2 ORDER BY 1", &[Value::Int(1), Value::Text("90".into())])
        .unwrap();
    let Output::Rows { rows, columns } = &out[0] else { panic!() };
    assert_eq!(columns[0].name, "name");
    assert_eq!(rows, &vec![vec![Value::Text("ada".into())]]);
}

#[test]
fn survives_reopening() {
    let vfs = SimVfs::new();
    let path = Path::new("r.db");
    {
        let db = Database::open_with(&vfs, path, Options::default()).unwrap();
        let mut s = db.session();
        setup(&mut s);
        s.execute("CREATE INDEX by_dept ON emp (dept)").unwrap();
    }
    let db = Database::open_with(&vfs, path, Options::default()).unwrap();
    let mut s = db.session();
    assert_eq!(flat(q(&mut s, "SELECT count(*) FROM emp WHERE dept = 1")), "2");
    db.mvcc().check().unwrap();
}
