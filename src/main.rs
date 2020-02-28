use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::RegexSetBuilder;
use serde_derive::Deserialize;
use toml;

#[derive(Debug, Default, Deserialize)]
struct Config {
    repos: Vec<Repo>,
    #[serde(default)]
    harness_directive: Option<String>,
    #[serde(default)]
    directive: Option<String>,
    #[serde(default)]
    included_tests: Vec<String>,
    #[serde(default)]
    excluded_tests: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Repo {
    name: String,
    url: String,
    #[serde(default)]
    skip_merge: bool,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    directive: Option<String>,
    #[serde(default)]
    included_tests: Vec<String>,
    #[serde(default)]
    excluded_tests: Vec<String>,
}

#[derive(Debug)]
enum Merge {
    Unmerged,
    Merged,
    Conflicted,
}

#[derive(Debug)]
struct Status {
    commit: String,
    merged: Merge,
    interpreter: bool,
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
                return;
            }

            paths.push(path);
        }
    }

    find(dir, &mut paths);
    paths
}

fn diff(a: &Path, b: &Path) -> bool {
    match (fs::read_to_string(a), fs::read_to_string(b)) {
        (Ok(a), Ok(b)) => a != b,
        _ => true,
    }
}

fn write_string<P: AsRef<Path>>(path: P, text: &str) -> Result<(), Error> {
    let path = path.as_ref();
    let dir = path.parent().unwrap();
    let _ = fs::create_dir_all(dir);
    fs::write(path, text.as_bytes())?;
    Ok(())
}

fn main() {
    let config: Config =
        toml::from_str(&fs::read_to_string("config.toml").expect("failed to read config.toml"))
            .expect("invalid config.toml");

    clean();

    let mut successes = Vec::new();
    let mut failures = Vec::new();

    for repo in &config.repos {
        println!("@@ {:?}", repo);

        match build_repo(repo, &config) {
            Ok(status) => successes.push((repo, status)),
            Err(err) => failures.push((repo, err)),
        };
    }
    println!("@@ done");

    let mut results = String::new();
    for (repo, status) in &successes {
        write!(
            results,
            "{}: ({} {}) {}",
            repo.name,
            match status.merged {
                Merge::Unmerged => "unmerged",
                Merge::Merged => "merged",
                Merge::Conflicted => "conflicted",
            },
            if status.interpreter {
                "building"
            } else {
                "broken"
            },
            status.commit
        )
        .unwrap();
    }
    for (repo, err) in &failures {
        writeln!(results, "{}: (failure) {:?}", repo.name, err).unwrap();
    }

    println!("{}", results);
    write_string("tests/proposals", &results).unwrap();
}

fn clean() {
    println!("@@ clean");
    let _ = fs::create_dir("./repos");
    let _ = fs::remove_dir_all("./tests");
}

fn build_repo(repo: &Repo, config: &Config) -> Result<Status, Error> {
    let repo_dir = format!("repos/{}", repo.name);
    let upstream_commit = repo
        .commit
        .as_ref()
        .map(|x| x.as_str())
        .unwrap_or("origin/master");

    // Initialize repo if it doesn't exist
    if !Path::new(&repo_dir).exists() {
        fs::create_dir(&repo_dir).unwrap();
        if let Some(parent) = &repo.parent {
            let parent = config
                .repos
                .iter()
                .find(|x| &x.name == parent)
                .expect("invalid parent name");
            let parent_dir = format!("repos/{}", parent.name);
            assert!(Path::new(&parent_dir).exists());

            // TODO: "--reference", &parent_dir,
            //   This would minimize the network traffic and repo size, but I
            //   was running into some corruption issues with it.
            run("git", &["clone", &repo.url, &repo_dir])?;
            run(
                "git",
                &["-C", &repo_dir, "remote", "add", "parent", &parent.url],
            )?;
            run("git", &["-C", &repo_dir, "branch", "try-merge"])?;
        } else {
            run("git", &["clone", &repo.url, &repo_dir])?;
        }
    }

    // Change to the repo dir for convenience
    {
        let _cd = change_dir(&repo_dir);

        // Update repo to latest changes
        run("git", &["checkout", "master"])?;
        run("git", &["fetch", "origin"])?;
        run("git", &["reset", upstream_commit, "--hard"])?;

        let mut merged = Merge::Unmerged;
        if let Some(parent) = &repo.parent {
            if !repo.skip_merge {
                // Try to merge with master
                run("git", &["fetch", "parent"])?;
                run("git", &["checkout", "try-merge"])?;
                run("git", &["reset", upstream_commit, "--hard"])?;
                let hash = run("git", &["log", "--pretty=%h", "-n", "1"])?;
                let message = format!("Merging {}:{}with {}", repo.name, hash, parent);

                // Try to merge and ignore merge conflicts in the document directory.
                if !run("git", &["merge", "-q", "parent/master", "-m", &message]).is_ok() {
                    if !run("git", &["checkout", "--ours", "document"]).is_ok()
                        || !run("git", &["add", "document"]).is_ok()
                        || !run("git", &["-c", "core.editor=true", "merge", "--continue"]).is_ok()
                    {
                        // Reset to master if we failed
                        println!("! failed to merge {}", repo.name);
                        run("git", &["merge", "--abort"])?;
                        run("git", &["reset", upstream_commit, "--hard"])?;
                        merged = Merge::Conflicted;
                    } else {
                        merged = Merge::Merged;
                    }
                } else {
                    merged = Merge::Merged;
                }
            }
        }

        // Build tests
        let build_tests = || {
            let _ = fs::remove_dir_all("./js");
            let _ = fs::remove_dir_all("./wpt");
            run(
                "test/build.py",
                &["--use-sync", "--js", "./js", "--html", "./wpt"],
            )
        };

        let mut interpreter = true;
        if build_tests().is_err() {
            if repo.parent.is_some() {
                println!("@@ failed to compile, trying again on master/pinned commit");
                run("git", &["reset", upstream_commit, "--hard"])?;
                interpreter = build_tests().is_ok();
            } else {
                interpreter = false;
            }
        }

        // Get the final commit message we ended up on
        let commit = run("git", &["log", "--oneline", "-n", "1"])?;

        if !interpreter {
            println!("@@ failed to compile, won't emit js/html");
        }

        // Compute the source files that changed, use that filter the files we
        // copy over. We can't compare the generated tests, because for a
        // generated WPT we need to copy both the .js and .html even if only
        // one of those is different from the master.
        let mut included_files = Vec::new();
        for test_path in find("test/core") {
            let test_name = test_path.file_name().unwrap().to_str().unwrap().to_owned();

            if let Some(parent) = &repo.parent {
                let parent_test_path = Path::new("../").join(parent).join(&test_path);
                if diff(&test_path, &parent_test_path) {
                    included_files.push(test_name);
                }
            } else {
                included_files.push(test_name);
            }
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

        let copy_tests = |test_dir, test_name| {
            let _cd = change_dir(test_dir);

            for path in find("./") {
                let path_str = path.to_str().unwrap();

                if !include.is_match(path_str) || exclude.is_match(path_str) {
                    continue;
                }

                let out_path = Path::new("../../../tests/")
                    .join(test_name)
                    .join(&repo.name)
                    .join(&path);
                let out_dir = out_path.parent().unwrap();
                let _ = fs::create_dir_all(out_dir);
                fs::copy(path, out_path).unwrap();
            }
        };

        copy_tests("test/core", "wast");
        if interpreter {
            copy_tests("wpt", "wpt");
            copy_tests("js", "js");

            // Write directives files
            if let Some(harness_directive) = &config.harness_directive {
                let directives_path = Path::new("../../tests/js")
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
                let directives_path = Path::new("../../tests/js")
                    .join(&repo.name)
                    .join("directives.txt");
                write_string(&directives_path, &directives)?;
            }
        }

        Ok(Status {
            commit,
            merged,
            interpreter,
        })
    }
}