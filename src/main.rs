use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::RegexSetBuilder;
use serde_derive::{Deserialize, Serialize};
use toml;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    harness_directive: Option<String>,
    #[serde(default)]
    directive: Option<String>,
    #[serde(default)]
    included_tests: Vec<String>,
    #[serde(default)]
    excluded_tests: Vec<String>,
    repos: Vec<Repo>,
}

impl Config {
    fn find_repo_mut(&mut self, name: &str) -> Option<&mut Repo> {
        self.repos.iter_mut().find(|x| &x.name == name)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Repo {
    name: String,
    url: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    directive: Option<String>,
    #[serde(default)]
    included_tests: Vec<String>,
    #[serde(default)]
    excluded_tests: Vec<String>,
    #[serde(default)]
    skip_wast: bool,
    #[serde(default)]
    skip_wpt: bool,
    #[serde(default)]
    skip_js: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Lock {
    repos: Vec<LockRepo>,
}

impl Lock {
    fn find_commit(&self, name: &str) -> Option<&str> {
        self.repos
            .iter()
            .find(|x| &x.name == name)
            .map(|x| x.commit.as_ref())
    }

    fn set_commit(&mut self, name: &str, commit: &str) {
        if let Some(lock) = self.repos.iter_mut().find(|x| &x.name == name) {
            lock.commit = commit.to_owned();
        } else {
            self.repos.push(LockRepo {
                name: name.to_owned(),
                commit: commit.to_owned(),
            });
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LockRepo {
    name: String,
    commit: String,
}

#[derive(Debug)]
enum Merge {
    Unmerged,
    Merged,
    Conflicted,
}

#[derive(Debug)]
struct Status {
    commit_base_hash: String,
    commit_final_message: String,
    merged: Merge,
    built: bool,
}

#[derive(Debug)]
enum Error {
    Io(std::io::Error),
    Utf8(std::string::FromUtf8Error),
    FailedProcess(String, String, String),
}

impl From<std::io::Error> for Error {
    fn from(other: std::io::Error) -> Error {
        Error::Io(other)
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(other: std::string::FromUtf8Error) -> Error {
        Error::Utf8(other)
    }
}

fn run(name: &str, args: &[&str]) -> Result<String, Error> {
    println!("@ {} {:?}", name, args);
    let output = Command::new(name).args(args).output()?;
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;

    print!("{}", stdout);
    eprint!("{}", stderr);
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(Error::FailedProcess(name.to_owned(), stdout, stderr))
    }
}

fn change_dir(dir: &str) -> impl Drop {
    #[must_use]
    struct Reset {
        previous: PathBuf,
    }
    impl Drop for Reset {
        fn drop(&mut self) {
            println!("@ cd {}", self.previous.display());
            env::set_current_dir(&self.previous).unwrap()
        }
    }

    let previous = Reset {
        previous: env::current_dir().unwrap(),
    };
    println!("@ cd {}", dir);
    env::set_current_dir(dir).unwrap();
    previous
}

fn find(dir: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    fn find(dir: &str, paths: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap().map(|x| x.unwrap()) {
            let path = entry.path();

            if entry.file_type().unwrap().is_dir() {
                find(path.to_str().unwrap(), paths);
            } else {
                paths.push(path);
            }
        }
    }

    find(dir, &mut paths);
    paths
}

fn write_string<P: AsRef<Path>>(path: P, text: &str) -> Result<(), Error> {
    let path = path.as_ref();
    let dir = path.parent().unwrap();
    let _ = fs::create_dir_all(dir);
    fs::write(path, text.as_bytes())?;
    Ok(())
}

const SPECS_DIR: &str = "specs/";

fn main() {
    let mut config: Config =
        toml::from_str(&fs::read_to_string("config.toml").expect("failed to read config.toml"))
            .expect("invalid config.toml");
    let mut lock: Lock = if Path::new("config-lock.toml").exists() {
        toml::from_str(
            &fs::read_to_string("config-lock.toml").expect("failed to read config-lock.toml"),
        )
        .expect("invalid config-lock.toml")
    } else {
        Lock::default()
    };

    clean();

    if !Path::new(SPECS_DIR).exists() {
        fs::create_dir(SPECS_DIR).unwrap();
        run("git", &["-C", SPECS_DIR, "init"]).unwrap();
    }

    let mut successes = Vec::new();
    let mut failures = Vec::new();

    for repo in &config.repos {
        println!("@@ {:?}", repo);

        match build_repo(repo, &config, &lock) {
            Ok(status) => successes.push((repo.name.clone(), status)),
            Err(err) => failures.push((repo.name.clone(), err)),
        };
    }

    if !failures.is_empty() {
        println!("@@ failed!");
        for (name, err) in &failures {
            println!("{}: (failure) {:?}", name, err);
        }
        std::process::exit(1);
    }

    println!("@@ done!");
    for (name, status) in &successes {
        let repo = config.find_repo_mut(&name).unwrap();
        lock.set_commit(&name, &status.commit_base_hash);

        println!(
            "{}: ({} {}) {}",
            repo.name,
            match status.merged {
                Merge::Unmerged => "unmerged",
                Merge::Merged => "merged",
                Merge::Conflicted => "conflicted",
            },
            if status.built { "building" } else { "broken" },
            status.commit_final_message
        );
    }

    write_string("config-lock.toml", &toml::to_string_pretty(&lock).unwrap()).unwrap();
}

fn clean() {
    println!("@@ clean");
    let _ = fs::remove_dir_all("./tests");
}

fn build_repo(repo: &Repo, config: &Config, lock: &Lock) -> Result<Status, Error> {
    let _cd = change_dir(SPECS_DIR);

    let remote_name = &repo.name;
    let remote_url = &repo.url;
    let branch_upstream = format!("{}/master", repo.name);
    let branch_base = repo.name.clone();

    // Initialize our remote and branches if they don't exist
    let remotes = run("git", &["remote"])?;
    if !remotes.lines().any(|x| x == repo.name) {
        run("git", &["remote", "add", remote_name, &remote_url])?;
        run("git", &["fetch", remote_name])?;
        run(
            "git",
            &["branch", "--track", &branch_base, &branch_upstream],
        )?;
    }

    // Fetch the latest changes for this repo
    run("git", &["fetch", remote_name])?;

    // Checkout the pinned commit, if any, and get the absolute commit hash
    let base_treeish = lock.find_commit(&repo.name).unwrap_or(&branch_upstream);
    run("git", &["checkout", &branch_base])?;
    run("git", &["reset", base_treeish, "--hard"])?;
    let commit_base_hash = run("git", &["log", "--pretty=%h", "-n", "1"])?
        .trim()
        .to_owned();

    // Try to merge with parent repo, if specified
    let merged = if let Some(parent) = repo.parent.as_ref() {
        run("git", &["fetch", parent])?;

        // Try to merge
        let message = format!("Merging {}:{}with {}", repo.name, commit_base_hash, parent);
        if !run("git", &["merge", "-q", parent, "-m", &message]).is_ok() {
            // Ignore merge conflicts in the document directory.
            if !run("git", &["checkout", "--ours", "document"]).is_ok()
                || !run("git", &["add", "document"]).is_ok()
                || !run("git", &["-c", "core.editor=true", "merge", "--continue"]).is_ok()
            {
                // Reset to master if we failed
                println!("! failed to merge {}", repo.name);
                run("git", &["merge", "--abort"])?;
                run("git", &["reset", &commit_base_hash, "--hard"])?;
                Merge::Conflicted
            } else {
                Merge::Merged
            }
        } else {
            Merge::Merged
        }
    } else {
        Merge::Unmerged
    };

    // Try to build the test suite on this commit. This may fail due to merging
    // with a parent repo, in which case we will try again in an unmerged state.
    let build_tests = || {
        let _ = fs::remove_dir_all("./js");
        let _ = fs::remove_dir_all("./wpt");
        run(
            "test/build.py",
            &["--use-sync", "--js", "./js", "--html", "./wpt"],
        )
    };

    let mut built = true;
    if build_tests().is_err() {
        if repo.parent.is_some() {
            println!("@@ failed to compile, trying again on master/pinned commit");
            run("git", &["reset", &commit_base_hash, "--hard"])?;
            built = build_tests().is_ok();
        } else {
            println!("@@ failed to compile, won't emit js/html");
            built = false;
        }
    }

    // Get the final commit message we ended up on
    let commit_final_message = run("git", &["log", "--oneline", "-n", "1"])?;

    // Compute the source files that changed, use that filter the files we
    // copy over. We can't compare the generated tests, because for a
    // generated WPT we need to copy both the .js and .html even if only
    // one of those is different from the master.
    let mut included_files = Vec::new();
    let tests_changed = if let Some(parent) = repo.parent.as_ref() {
        run(
            "git",
            &["diff", "--name-only", &branch_base, &parent, "test/core"],
        )?
        .lines()
        .map(|x| PathBuf::from(x))
        .collect()
    } else {
        find("test/core")
    };

    for test_path in tests_changed {
        if test_path.extension().map(|x| x.to_str().unwrap()) != Some("wast") {
            continue;
        }

        let test_name = test_path.file_name().unwrap().to_str().unwrap().to_owned();
        included_files.push(test_name);
    }
    println!("@@ changed files {:?}", included_files);

    // Include the harness/ directory unconditionally
    included_files.push("harness/".to_owned());
    // Also include manually specified files
    included_files.extend_from_slice(&repo.included_tests);

    // Exclude files specified from the config and repo
    let mut excluded_files = Vec::new();
    excluded_files.extend_from_slice(&config.excluded_tests);
    excluded_files.extend_from_slice(&repo.excluded_tests);

    // Generate a regex set of the files to include or exclude
    let include = RegexSetBuilder::new(&included_files).build().unwrap();
    let exclude = RegexSetBuilder::new(&excluded_files).build().unwrap();

    let copy_tests = |src_dir, dst_dir, test_name| {
        for path in find(src_dir) {
            let path_str = path.to_str().unwrap();

            if !include.is_match(path_str) || exclude.is_match(path_str) {
                continue;
            }

            let out_path = Path::new(dst_dir)
                .join(test_name)
                .join(&repo.name)
                .join(&path);
            let out_dir = out_path.parent().unwrap();
            let _ = fs::create_dir_all(out_dir);
            fs::copy(path, out_path).unwrap();
        }
    };

    if !repo.skip_wast {
        copy_tests("test/core", "../tests", "wast");
    }
    if built && !repo.skip_wpt {
        copy_tests("wpt", "../tests", "wpt");
    }
    if built && !repo.skip_js {
        copy_tests("js", "../tests", "js");

        // Write directives files
        if let Some(harness_directive) = &config.harness_directive {
            let directives_path = Path::new("../tests/js")
                .join(&repo.name)
                .join("harness/directives.txt");
            write_string(&directives_path, harness_directive)?;
        }
        let directives = format!(
            "{}{}",
            config.directive.as_ref().map(|x| x.as_str()).unwrap_or(""),
            repo.directive.as_ref().map(|x| x.as_str()).unwrap_or("")
        );
        if !directives.is_empty() {
            let directives_path = Path::new("../tests/js")
                .join(&repo.name)
                .join("directives.txt");
            write_string(&directives_path, &directives)?;
        }
    }

    Ok(Status {
        commit_final_message,
        commit_base_hash,
        merged,
        built,
    })
}
