//! An export of exactly what two apps are built from. `copybera closure` reads an
//! origin's Cargo manifests at a revision and prints the include list of the named binaries' source closure (each
//! crate folder their normal and build path dependencies reach, through `workspace = true`, `[patch]` and detached
//! workspaces, the workspace roots' manifests, locks and cargo config, and the files outside a crate its sources name
//! by a relative path: an `include_str!`, a build script's input). A workflow whose include is that list alone, with
//! `[cascade] rust = "auto"`, keeps both workspaces loadable and building; `closure --check` names what a workflow's
//! include misses once the origin grows a new dependency.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_copybera"))
}

struct Fixture {
    _tmp: TempDir,
    home: PathBuf,
    root: PathBuf,
    origin: PathBuf,
    dest: PathBuf,
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn git(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn manifest(name: &str, extra: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}")
}

/// Two binaries in two workspaces: `crates/app` is a member of the root workspace, `apps/shadow` a detached workspace of
/// its own (excluded by the root, as the GPUI apps are). Both reach `crates/shared`; the app reaches `crates/proto` through
/// `[workspace.dependencies]` and `crates/extra` only as a dev-dependency; the shadow app's build script reads a schema
/// two folders up and its main includes a banner. `crates/extra`, `crates/extra_user`, `crates/patched`'s neighbour and
/// `docs/` are outside the closure.
fn closure_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join(".gitconfig"),
        "[user]\n    name = Fixture\n    email = fixture@example.invalid\n[init]\n    defaultBranch = main\n",
    )
    .unwrap();
    let origin = root.join("origin");
    let dest = root.join("dest");
    git(
        &home,
        &root,
        &["init", "-q", "-b", "main", origin.to_str().unwrap()],
    );
    git(
        &home,
        &root,
        &["init", "-q", "--bare", "-b", "main", dest.to_str().unwrap()],
    );

    write(
        &origin,
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\n    \"crates/app\",\n    \"crates/shared\",\n    \"crates/proto\",\n    \
         \"crates/extra\",\n    \"crates/extra_user\",\n]\nexclude = [\"apps/shadow\"]\n\n[workspace.dependencies]\n\
         proto = { path = \"crates/proto\" }\nextra = { path = \"crates/extra\" }\n",
    );
    write(
        &origin,
        ".cargo/config.toml",
        "[net]\ngit-fetch-with-cli = true\n",
    );
    write(
        &origin,
        "crates/app/Cargo.toml",
        &manifest(
            "app",
            "\n[dependencies]\nshared = { path = \"../shared\" }\nproto.workspace = true\n\n[dev-dependencies]\nextra.workspace = true\n",
        ),
    );
    write(
        &origin,
        "crates/app/src/main.rs",
        "fn main() {\n    shared::hello();\n    proto::wire();\n}\n",
    );
    write(&origin, "crates/shared/Cargo.toml", &manifest("shared", ""));
    write(&origin, "crates/shared/src/lib.rs", "pub fn hello() {}\n");
    write(&origin, "crates/proto/Cargo.toml", &manifest("proto", ""));
    write(&origin, "crates/proto/src/lib.rs", "pub fn wire() {}\n");
    write(&origin, "crates/extra/Cargo.toml", &manifest("extra", ""));
    write(&origin, "crates/extra/src/lib.rs", "pub fn extra() {}\n");
    write(
        &origin,
        "crates/extra_user/Cargo.toml",
        &manifest("extra_user", "\n[dependencies]\nextra.workspace = true\n"),
    );
    write(
        &origin,
        "crates/extra_user/src/lib.rs",
        "pub fn user() { extra::extra() }\n",
    );
    write(
        &origin,
        "apps/shadow/Cargo.toml",
        &manifest(
            "shadow",
            "build = \"build.rs\"\n\n[workspace]\n\n[dependencies]\nshared = { path = \"../../crates/shared\" }\n\n\
             [target.'cfg(unix)'.dependencies]\npatched = { path = \"../../vendor/patched\" }\n",
        ),
    );
    write(
        &origin,
        "apps/shadow/build.rs",
        "fn main() {\n    let schema = std::fs::read_to_string(\"../../assets/schema/v1.txt\").expect(\"the schema\");\n    \
         println!(\"cargo:rustc-env=SCHEMA_LEN={}\", schema.len());\n}\n",
    );
    write(
        &origin,
        "apps/shadow/src/main.rs",
        "const BANNER: &str = include_str!(\"../../../assets/banner.txt\");\n\nfn main() {\n    shared::hello();\n    \
         #[cfg(unix)]\n    patched::patched();\n    print!(\"{BANNER}{}\", env!(\"SCHEMA_LEN\"));\n}\n",
    );
    write(
        &origin,
        "vendor/patched/Cargo.toml",
        &manifest("patched", ""),
    );
    write(
        &origin,
        "vendor/patched/src/lib.rs",
        "pub fn patched() {}\n",
    );
    write(&origin, "vendor/other/Cargo.toml", &manifest("other", ""));
    write(&origin, "vendor/other/src/lib.rs", "pub fn other() {}\n");
    write(&origin, "assets/schema/v1.txt", "schema one\n");
    write(&origin, "assets/banner.txt", "banner\n");
    write(&origin, "assets/unused.txt", "not built from\n");
    write(&origin, "docs/notes.md", "notes\n");
    git(&home, &origin, &["add", "-A"]);
    git(&home, &origin, &["commit", "-q", "-m", "two apps"]);
    Fixture {
        _tmp: tmp,
        home,
        root,
        origin,
        dest,
    }
}

fn copybera(fx: &Fixture, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(bin())
        .args(args)
        .current_dir(&fx.root)
        .env("HOME", &fx.home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn closure_args(fx: &Fixture) -> Vec<&str> {
    vec![
        "closure",
        fx.origin.to_str().unwrap(),
        "--ref",
        "main",
        "--cargo",
        "crates/app",
        "--cargo",
        "apps/shadow",
    ]
}

fn workflow(fx: &Fixture, include: &str) -> PathBuf {
    let path = fx.root.join("apps.toml");
    let target = fx.root.join("verify-target");
    let target = target.display();
    fs::write(
        &path,
        format!(
            "[workflow]\nname = \"apps\"\nmode = \"squash\"\nreceipt = \"apps.receipt.json\"\n\n\
             [origin]\npath = \"origin\"\nref = \"main\"\n\n[destination]\npath = \"dest\"\nfetch = \"main\"\npush = \"main\"\n\n\
             [paths.origin]\n{include}\nexclude = []\n\n[paths.destination]\ninclude = [\"**\"]\nexclude = []\n\n\
             [authoring]\ndefault = {{ name = \"Fixture Authors\", email = \"authors@example.invalid\" }}\n\n\
             [cascade]\nrust = \"auto\"\n\n[verify]\ncommands = [\n    \
             [\"cargo\", \"metadata\", \"--format-version\", \"1\", \"--offline\"],\n    \
             [\"cargo\", \"check\", \"--workspace\", \"--offline\", \"--quiet\"],\n    \
             [\"cargo\", \"metadata\", \"--format-version\", \"1\", \"--offline\", \"--manifest-path\", \"apps/shadow/Cargo.toml\"],\n    \
             [\"cargo\", \"check\", \"--offline\", \"--quiet\", \"--manifest-path\", \"apps/shadow/Cargo.toml\"],\n]\n\
             dir = \"verify-tree\"\non_fail = \"refuse\"\nenv = {{ CARGO_TARGET_DIR = \"{target}\" }}\n"
        ),
    )
    .unwrap();
    path
}

#[test]
fn closure_lists_what_two_binaries_are_built_from() {
    let fx = closure_fixture();
    let (ok, stdout, stderr) = copybera(&fx, &closure_args(&fx));
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let include = stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"'))
        .filter_map(|l| l.split_once('"').map(|(p, _)| p.to_string()))
        .collect::<Vec<_>>();
    for want in [
        ".cargo/config.toml",
        "Cargo.toml",
        "apps/shadow/**",
        "assets/banner.txt",
        "assets/schema/v1.txt",
        "crates/app/**",
        "crates/proto/**",
        "crates/shared/**",
        "vendor/patched/**",
    ] {
        assert!(
            include.iter().any(|p| p == want),
            "{want} missing\n{stdout}"
        );
    }
    for unwanted in ["crates/extra", "vendor/other", "assets/unused.txt", "docs/"] {
        assert!(
            !include.iter().any(|p| p.starts_with(unwanted)),
            "{unwanted} should be outside the closure\n{stdout}"
        );
    }
    assert!(stdout.starts_with("include = ["), "{stdout}");
    assert!(
        stderr.contains("closure: 5 crates in 2 workspaces"),
        "{stderr}"
    );
}

#[test]
fn a_closure_only_workflow_keeps_both_workspaces_loadable() {
    let fx = closure_fixture();
    let (ok, include, stderr) = copybera(&fx, &closure_args(&fx));
    assert!(ok, "stderr={stderr}");
    let wf = workflow(&fx, &include);
    let (ok, stdout, stderr) = copybera(&fx, &["migrate", wf.to_str().unwrap()]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    assert!(stdout.contains("verify: 4 of 4 passed"), "{stdout}");
    let listing = git(
        &fx.home,
        &fx.dest,
        &["ls-tree", "-r", "--name-only", "main"],
    );
    for kept in [
        "crates/app/src/main.rs",
        "crates/shared/src/lib.rs",
        "crates/proto/src/lib.rs",
        "apps/shadow/build.rs",
        "apps/shadow/src/main.rs",
        "vendor/patched/src/lib.rs",
        "assets/schema/v1.txt",
        "assets/banner.txt",
    ] {
        assert!(
            listing.lines().any(|l| l == kept),
            "{kept} missing\n{listing}"
        );
    }
    for gone in ["crates/extra", "vendor/other", "assets/unused.txt", "docs/"] {
        assert!(!listing.contains(gone), "{gone} exported\n{listing}");
    }
    let root = git(&fx.home, &fx.dest, &["show", "main:Cargo.toml"]);
    assert!(!root.contains("crates/extra"), "{root}");
    assert!(
        root.contains("\"crates/app\"") && root.contains("proto = "),
        "{root}"
    );
    let app = git(&fx.home, &fx.dest, &["show", "main:crates/app/Cargo.toml"]);
    assert!(
        !app.contains("extra"),
        "the dev-dependency is unlinked\n{app}"
    );
    // The cascade had nothing of the closure's to strip: every file it acted on is outside it.
    assert!(
        stdout.contains("rust cascade: 0 files (0 crates)"),
        "{stdout}"
    );
}

#[test]
fn closure_check_names_what_a_workflows_include_misses() {
    let fx = closure_fixture();
    let (ok, include, stderr) = copybera(&fx, &closure_args(&fx));
    assert!(ok, "stderr={stderr}");
    let wf = workflow(&fx, &include);
    let mut args = closure_args(&fx);
    args.extend(["--check", wf.to_str().unwrap()]);
    let (ok, stdout, stderr) = copybera(&fx, &args);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    assert!(stdout.contains("closure check: covered"), "{stdout}");

    // The shared crate grows a dependency on a crate outside the old closure.
    write(
        &fx.origin,
        "crates/shared/Cargo.toml",
        &manifest(
            "shared",
            "\n[dependencies]\nextra = { path = \"../extra\" }\n",
        ),
    );
    git(
        &fx.home,
        &fx.origin,
        &["commit", "-q", "-am", "shared needs extra"],
    );
    let (ok, stdout, stderr) = copybera(&fx, &args);
    assert!(!ok, "a missed crate refuses\nstdout={stdout}");
    assert!(stdout.contains("missing crates/extra/**"), "{stdout}");
    assert!(
        stderr.contains("closure check: 1 entry missing"),
        "{stderr}"
    );
}
