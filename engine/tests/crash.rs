//! The crash test, briefly: kill the worker process 8 times mid-write and check the database
//! after each kill (the full run is `lodestone crash-check <db> 300`).

use std::process::Command;

#[test]
fn killed_mid_write_the_database_recovers() {
    let dir = std::env::temp_dir().join(format!("lodestone-crash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lodestone"))
        .args(["crash-check"])
        .arg(dir.join("c.db"))
        .args(["8", "--seed", "7"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("8 kills, 0 failed checks"), "{text}");
}
