//! Test fixtures: source repositories built with the git CLI, with fixed
//! identities and dates so results are deterministic.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use sourcegull_gitstore::{GitStore, GitStoreConfig, Remote, RemoteAuth};

/// A working repository used as the "code host" side of clone/fetch tests.
pub struct SourceRepo {
    pub dir: tempfile::TempDir,
    tick: std::cell::Cell<i64>,
}

impl SourceRepo {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let s = Self {
            dir,
            tick: std::cell::Cell::new(0),
        };
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "commit.gpgsign", "false"]);
        s
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn remote(&self) -> Remote {
        Remote::new(&self.path().display().to_string(), RemoteAuth::None)
    }

    /// Runs git in the work tree and returns trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        let t = self.tick.get() + 1;
        self.tick.set(t);
        let date = format!("{} +0000", 1_700_000_000 + t * 60);
        let out = Command::new("git")
            .args(args)
            .current_dir(self.path())
            .env("GIT_AUTHOR_NAME", "Ann Author")
            .env("GIT_AUTHOR_EMAIL", "ann@example.com")
            .env("GIT_COMMITTER_NAME", "Cid Committer")
            .env("GIT_COMMITTER_EMAIL", "cid@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", self.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    pub fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        let p = self.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Writes files and commits them; returns the commit id.
    pub fn commit(&self, msg: &str, files: &[(&str, &str)]) -> String {
        for (p, c) in files {
            self.write(p, c);
        }
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }
}

/// A source repo with a small but varied history:
///
/// ```text
/// c1 README, src/main.rs, src/lib.rs, docs/guide.md
/// c2 edit src/main.rs, add bin.dat (binary)
/// feature: f1 edit src/lib.rs
/// main:    c3 rename docs/guide.md -> docs/manual.md, delete bin.dat
/// m  merge feature into main
/// c4 edit src/main.rs again; tag v1.0 (annotated) on c4, light tag on c1
/// ```
pub fn history_repo() -> SourceRepo {
    let s = SourceRepo::new();
    let main_v1 = "fn main() {\n    println!(\"hello\");\n}\n";
    let lib_v1 = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let guide = (1..=30).map(|i| format!("line {i}\n")).collect::<String>();
    s.commit(
        "c1: initial",
        &[
            ("README.md", "# Demo\n"),
            ("src/main.rs", main_v1),
            ("src/lib.rs", lib_v1),
            ("docs/guide.md", &guide),
        ],
    );
    s.git(&["tag", "light", "HEAD"]);
    let main_v2 = "fn main() {\n    println!(\"hello, world\");\n    run();\n}\n";
    s.write("bin.dat", [0u8, 1, 2, 3, 0, 255]);
    s.commit("c2: greet the world", &[("src/main.rs", main_v2)]);
    s.git(&["checkout", "-q", "-b", "feature"]);
    s.commit(
        "f1: subtract",
        &[(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )],
    );
    s.git(&["checkout", "-q", "main"]);
    s.git(&["mv", "docs/guide.md", "docs/manual.md"]);
    s.git(&["rm", "-q", "bin.dat"]);
    s.commit("c3: rename guide", &[]);
    s.git(&["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
    s.commit(
        "c4: run",
        &[(
            "src/main.rs",
            "fn main() {\n    println!(\"hello, world\");\n    run();\n}\n\nfn run() {}\n",
        )],
    );
    s.git(&["tag", "-a", "v1.0", "-m", "release 1.0"]);
    s
}

pub fn store() -> (tempfile::TempDir, Arc<GitStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(GitStoreConfig::new(dir.path())).unwrap();
    (dir, Arc::new(store))
}

/// Runs git against a bare repo and returns stdout.
pub fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
