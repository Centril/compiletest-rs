// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "lib"]

#![cfg_attr(not(feature = "norustc"), feature(rustc_private))]
#![cfg_attr(not(feature = "stable"), feature(test))]
#![cfg_attr(not(feature = "stable"), feature(slice_rotate))]

#![forbid(unused_imports)]
#![forbid(non_snake_case)]
#![forbid(unused_variables)]
#![forbid(unreachable_patterns)]
#![forbid(dead_code)]


#[cfg(not(feature = "norustc"))]
extern crate rustc;

#[cfg(unix)]
extern crate libc;
extern crate test;

#[cfg(feature = "tmp")] extern crate tempfile;

#[macro_use]
extern crate log;
extern crate filetime;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use common::{TestPaths, Pretty};

use self::header::EarlyProps;

pub mod uidiff;
pub mod util;
mod json;
pub mod header;
pub mod runtest;
pub mod common;
pub mod errors;
mod read2;

pub use common::Config;

pub fn run_tests(config: &Config) {
    if config.target.contains("android") {
        // android debug-info test uses remote debugger
        // so, we test 1 thread at once.
        // also trying to isolate problems with adb_run_wrapper.sh ilooping
        env::set_var("RUST_TEST_THREADS","1");
    }

    let opts = test_opts(config);
    let tests = make_tests(config);
    // sadly osx needs some file descriptor limits raised for running tests in
    // parallel (especially when we have lots and lots of child processes).
    // For context, see #8904
    // unsafe { raise_fd_limit::raise_fd_limit(); }
    // Prevent issue #21352 UAC blocking .exe containing 'patch' etc. on Windows
    // If #11207 is resolved (adding manifest to .exe) this becomes unnecessary
    env::set_var("__COMPAT_LAYER", "RunAsInvoker");
    let res = test::run_tests_console(&opts, tests.into_iter().collect());
    match res {
        Ok(true) => {}
        Ok(false) => panic!("Some tests failed"),
        Err(e) => {
            println!("I/O failure during tests: {:?}", e);
        }
    }
}

pub fn test_opts(config: &Config) -> test::TestOpts {
    test::TestOpts {
        filter: config.filter.clone(),
        filter_exact: config.filter_exact,
        run_ignored: config.run_ignored,
        format: if config.quiet { test::OutputFormat::Terse } else { test::OutputFormat::Pretty },
        logfile: config.logfile.clone(),
        run_tests: true,
        bench_benchmarks: true,
        nocapture: match env::var("RUST_TEST_NOCAPTURE") {
            Ok(val) => &val != "0",
            Err(_) => false
        },
        color: test::AutoColor,
        test_threads: None,
        skip: vec![],
        list: false,
        options: test::Options::new(),
    }
}

pub fn make_tests(config: &Config) -> Vec<test::TestDescAndFn> {
    debug!("making tests from {:?}",
           config.src_base.display());
    let mut tests = Vec::new();
    collect_tests_from_dir(config,
                           &config.src_base,
                           &config.src_base,
                           &PathBuf::new(),
                           &mut tests)
        .unwrap();
    tests
}

fn collect_tests_from_dir(config: &Config,
                          base: &Path,
                          dir: &Path,
                          relative_dir_path: &Path,
                          tests: &mut Vec<test::TestDescAndFn>)
                          -> io::Result<()> {
    // Ignore directories that contain a file
    // `compiletest-ignore-dir`.
    for file in try!(fs::read_dir(dir)) {
        let file = try!(file);
        let name = file.file_name();
        if name == *"compiletest-ignore-dir" {
            return Ok(());
        }
    }

    // If we find a test foo/bar.rs, we have to build the
    // output directory `$build/foo` so we can write
    // `$build/foo/bar` into it. We do this *now* in this
    // sequential loop because otherwise, if we do it in the
    // tests themselves, they race for the privilege of
    // creating the directories and sometimes fail randomly.
    let build_dir = config.build_base.join(&relative_dir_path);
    fs::create_dir_all(&build_dir).unwrap();

    // Add each `.rs` file as a test, and recurse further on any
    // subdirectories we find, except for `aux` directories.
    let dirs = try!(fs::read_dir(dir));
    for file in dirs {
        let file = try!(file);
        let file_path = file.path();
        let file_name = file.file_name();
        if is_test(&file_name) {
            debug!("found test file: {:?}", file_path.display());
            // output directory `$build/foo` so we can write
            // `$build/foo/bar` into it. We do this *now* in this
            // sequential loop because otherwise, if we do it in the
            // tests themselves, they race for the privilege of
            // creating the directories and sometimes fail randomly.
            let build_dir = config.build_base.join(&relative_dir_path);
            fs::create_dir_all(&build_dir).unwrap();

            let paths = TestPaths {
                file: file_path,
                base: base.to_path_buf(),
                relative_dir: relative_dir_path.to_path_buf(),
            };
            tests.push(make_test(config, &paths))
        } else if file_path.is_dir() {
            let relative_file_path = relative_dir_path.join(file.file_name());
            if &file_name == "auxiliary" {
                // `aux` directories contain other crates used for
                // cross-crate tests. Don't search them for tests, but
                // do create a directory in the build dir for them,
                // since we will dump intermediate output in there
                // sometimes.
                let build_dir = config.build_base.join(&relative_file_path);
                fs::create_dir_all(&build_dir).unwrap();
            } else {
                debug!("found directory: {:?}", file_path.display());
                try!(collect_tests_from_dir(config,
                                       base,
                                       &file_path,
                                       &relative_file_path,
                                       tests));
            }
        } else {
            debug!("found other file/directory: {:?}", file_path.display());
        }
    }
    Ok(())
}

pub fn is_test(file_name: &OsString) -> bool {
    let file_name = file_name.to_str().unwrap();

    if !file_name.ends_with(".rs") {
        return false;
    }

    // `.`, `#`, and `~` are common temp-file prefixes.
    let invalid_prefixes = &[".", "#", "~"];
    !invalid_prefixes.iter().any(|p| file_name.starts_with(p))
}

pub fn make_test(config: &Config, testpaths: &TestPaths) -> test::TestDescAndFn {
    let early_props = EarlyProps::from_file(config, &testpaths.file);

    // The `should-fail` annotation doesn't apply to pretty tests,
    // since we run the pretty printer across all tests by default.
    // If desired, we could add a `should-fail-pretty` annotation.
    let should_panic = match config.mode {
        Pretty => test::ShouldPanic::No,
        _ => if early_props.should_fail {
            test::ShouldPanic::Yes
        } else {
            test::ShouldPanic::No
        }
    };

    test::TestDescAndFn {
        desc: test::TestDesc {
            name: make_test_name(config, testpaths),
            ignore: early_props.ignore,
            should_panic: should_panic,
            allow_fail: false,
        },
        testfn: make_test_closure(config, testpaths),
    }
}

fn stamp(config: &Config, testpaths: &TestPaths) -> PathBuf {
    let stamp_name = format!("{}-{}.stamp",
                             testpaths.file.file_name().unwrap()
                                           .to_str().unwrap(),
                             config.stage_id);
    config.build_base.canonicalize()
          .unwrap_or_else(|_| config.build_base.clone())
          .join(stamp_name)
}

pub fn make_test_name(config: &Config, testpaths: &TestPaths) -> test::TestName {
    // Convert a complete path to something like
    //
    //    run-pass/foo/bar/baz.rs
    let path =
        PathBuf::from(config.src_base.file_name().unwrap())
        .join(&testpaths.relative_dir)
        .join(&testpaths.file.file_name().unwrap());
    test::DynTestName(format!("[{}] {}", config.mode, path.display()))
}

pub fn make_test_closure(config: &Config, testpaths: &TestPaths) -> test::TestFn {
    let config = config.clone();
    let testpaths = testpaths.clone();
    test::DynTestFn(Box::new(move || {
        #[cfg(feature = "stable")]
        let config = config.clone();  // FIXME: why is this needed?
        runtest::run(config, &testpaths)
    }))
}
