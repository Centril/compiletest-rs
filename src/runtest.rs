// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use common::{Config, TestPaths};
use common::{CompileFail, Pretty, RunFail, RunPass};
use common::{RunMake, Ui};
use diff;
use errors::{self, ErrorKind, Error};
use json;
use header::TestProps;
use util::logv;

use std::env;
use std::ffi::OsString;
use std::fs::{self, File, create_dir_all};
use std::io::prelude::*;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, ExitStatus, Stdio, Child};
use std::str;

/// The name of the environment variable that holds dynamic library locations.
pub fn dylib_env_var() -> &'static str {
    if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(target_os = "haiku") {
        "LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

pub fn run(config: Config, testpaths: &TestPaths) {
    match &*config.target {

        "arm-linux-androideabi" | "armv7-linux-androideabi" | "aarch64-linux-android" => {
            if !config.adb_device_status {
                panic!("android device not available");
            }
        }

        _ => {}
    }

    if config.verbose {
        // We're going to be dumping a lot of info. Start on a new line.
        print!("\n\n");
    }
    debug!("running {:?}", testpaths.file.display());
    let base_props = TestProps::from_file(&testpaths.file, None, &config);

    let base_cx = TestCx { config: &config,
                           props: &base_props,
                           testpaths,
                           revision: None };
    base_cx.init_all();

    if base_props.revisions.is_empty() {
        base_cx.run_revision()
    } else {
        for revision in &base_props.revisions {
            let revision_props = TestProps::from_file(&testpaths.file,
                                                      Some(revision),
                                                      &config);
            let rev_cx = TestCx {
                config: &config,
                props: &revision_props,
                testpaths,
                revision: Some(revision)
            };
            rev_cx.run_revision();
        }
    }

    base_cx.complete_all();

    File::create(::stamp(&config, testpaths)).unwrap();
}

struct TestCx<'test> {
    config: &'test Config,
    props: &'test TestProps,
    testpaths: &'test TestPaths,
    revision: Option<&'test str>
}

impl<'test> TestCx<'test> {
    /// invoked once before any revisions have been processed
    fn init_all(&self) {
        assert!(self.revision.is_none(), "init_all invoked for a revision");
    }

    /// Code executed for each revision in turn (or, if there are no
    /// revisions, exactly once, with revision == None).
    fn run_revision(&self) {
        match self.config.mode {
            CompileFail => self.run_cfail_test(),
            RunFail => self.run_rfail_test(),
            RunPass => self.run_rpass_test(),
            Pretty => self.run_pretty_test(),
            RunMake => self.run_rmake_test(),
            Ui => self.run_ui_test(),
        }
    }

    /// Invoked after all revisions have executed.
    fn complete_all(&self) {
        assert!(self.revision.is_none(), "init_all invoked for a revision");
    }

    fn run_cfail_test(&self) {
        let proc_res = self.compile_test();

        if self.props.must_compile_successfully {
            if !proc_res.status.success() {
                self.fatal_proc_rec(
                    "test compilation failed although it shouldn't!",
                    &proc_res);
            }
        } else {
            if proc_res.status.success() {
                self.fatal_proc_rec(
                    &format!("{} test compiled successfully!", self.config.mode)[..],
                    &proc_res);
            }

            self.check_correct_failure_status(&proc_res);
        }

        let output_to_check = self.get_output(&proc_res);
        let expected_errors = errors::load_errors(&self.testpaths.file, self.revision);
        if !expected_errors.is_empty() {
            if !self.props.error_patterns.is_empty() {
                self.fatal("both error pattern and expected errors specified");
            }
            self.check_expected_errors(expected_errors, &proc_res);
        } else {
            self.check_error_patterns(&output_to_check, &proc_res);
        }

        self.check_no_compiler_crash(&proc_res);
        self.check_forbid_output(&output_to_check, &proc_res);
    }

    fn run_rfail_test(&self) {
        let proc_res = self.compile_test();

        if !proc_res.status.success() {
            self.fatal_proc_rec("compilation failed!", &proc_res);
        }

        let proc_res = self.exec_compiled_test();

        // The value our Makefile configures valgrind to return on failure
        const VALGRIND_ERR: i32 = 100;
        if proc_res.status.code() == Some(VALGRIND_ERR) {
            self.fatal_proc_rec("run-fail test isn't valgrind-clean!", &proc_res);
        }

        let output_to_check = self.get_output(&proc_res);
        self.check_correct_failure_status(&proc_res);
        self.check_error_patterns(&output_to_check, &proc_res);
    }

    fn get_output(&self, proc_res: &ProcRes) -> String {
        if self.props.check_stdout {
            format!("{}{}", proc_res.stdout, proc_res.stderr)
        } else {
            proc_res.stderr.clone()
        }
    }

    fn check_correct_failure_status(&self, proc_res: &ProcRes) {
        // The value the rust runtime returns on failure
        const RUST_ERR: i32 = 101;
        if proc_res.status.code() != Some(RUST_ERR) {
            self.fatal_proc_rec(
                &format!("failure produced the wrong error: {}",
                         proc_res.status),
                proc_res);
        }
    }

    fn run_rpass_test(&self) {
        let proc_res = self.compile_test();

        if !proc_res.status.success() {
            self.fatal_proc_rec("compilation failed!", &proc_res);
        }

        // FIXME(#41968): Move this check to tidy?
        let expected_errors = errors::load_errors(&self.testpaths.file, self.revision);
        assert!(expected_errors.is_empty(),
                "run-pass tests with expected warnings should be moved to ui/");

        let proc_res = self.exec_compiled_test();

        if !proc_res.status.success() {
            self.fatal_proc_rec("test run failed!", &proc_res);
        }
    }

    #[cfg(feature = "stable")]
    fn run_pretty_test(&self) {
        self.fatal("pretty-printing tests can only be used with nightly Rust".into());
    }

    #[cfg(not(feature = "stable"))]
    fn run_pretty_test(&self) {
        if self.props.pp_exact.is_some() {
            logv(self.config, "testing for exact pretty-printing".to_owned());
        } else {
            logv(self.config, "testing for converging pretty-printing".to_owned());
        }

        let rounds = match self.props.pp_exact { Some(_) => 1, None => 2 };

        let mut src = String::new();
        File::open(&self.testpaths.file).unwrap().read_to_string(&mut src).unwrap();
        let mut srcs = vec![src];

        let mut round = 0;
        while round < rounds {
            logv(self.config, format!("pretty-printing round {} revision {:?}",
                                      round, self.revision));
            let proc_res = self.print_source(srcs[round].to_owned(), &self.props.pretty_mode);

            if !proc_res.status.success() {
                self.fatal_proc_rec(&format!("pretty-printing failed in round {} revision {:?}",
                                             round, self.revision),
                                    &proc_res);
            }

            let ProcRes{ stdout, .. } = proc_res;
            srcs.push(stdout);
            round += 1;
        }

        let mut expected = match self.props.pp_exact {
            Some(ref file) => {
                let filepath = self.testpaths.file.parent().unwrap().join(file);
                let mut s = String::new();
                File::open(&filepath).unwrap().read_to_string(&mut s).unwrap();
                s
            }
            None => { srcs[srcs.len() - 2].clone() }
        };
        let mut actual = srcs[srcs.len() - 1].clone();

        if self.props.pp_exact.is_some() {
            // Now we have to care about line endings
            let cr = "\r".to_owned();
            actual = actual.replace(&cr, "").to_owned();
            expected = expected.replace(&cr, "").to_owned();
        }

        self.compare_source(&expected, &actual);

        // If we're only making sure that the output matches then just stop here
        if self.props.pretty_compare_only { return; }

        // Finally, let's make sure it actually appears to remain valid code
        let proc_res = self.typecheck_source(actual);
        if !proc_res.status.success() {
            self.fatal_proc_rec("pretty-printed source does not typecheck", &proc_res);
        }

        if !self.props.pretty_expanded { return }

        // additionally, run `--pretty expanded` and try to build it.
        let proc_res = self.print_source(srcs[round].clone(), "expanded");
        if !proc_res.status.success() {
            self.fatal_proc_rec("pretty-printing (expanded) failed", &proc_res);
        }

        let ProcRes{ stdout: expanded_src, .. } = proc_res;
        let proc_res = self.typecheck_source(expanded_src);
        if !proc_res.status.success() {
            self.fatal_proc_rec(
                "pretty-printed source (expanded) does not typecheck",
                &proc_res);
        }
    }

    fn print_source(&self, src: String, pretty_type: &str) -> ProcRes {
        let aux_dir = self.aux_output_dir_name();

        let mut rustc = Command::new(&self.config.rustc_path);
        rustc.arg("-")
            .args(&["-Z", &format!("unpretty={}", pretty_type)])
            .args(&["--target", &self.config.target])
            .arg("-L").arg(&aux_dir)
            .args(self.split_maybe_args(&self.config.target_rustcflags))
            .args(&self.props.compile_flags)
            .envs(self.props.exec_env.clone());

        self.compose_and_run(rustc,
                             self.config.compile_lib_path.to_str().unwrap(),
                             Some(aux_dir.to_str().unwrap()),
                             Some(src))
    }

    fn compare_source(&self,
                      expected: &str,
                      actual: &str) {
        if expected != actual {
            self.error("pretty-printed source does not match expected source");
            println!("\n\
expected:\n\
------------------------------------------\n\
{}\n\
------------------------------------------\n\
actual:\n\
------------------------------------------\n\
{}\n\
------------------------------------------\n\
\n",
                     expected, actual);
            panic!();
        }
    }

    fn typecheck_source(&self, src: String) -> ProcRes {
        let mut rustc = Command::new(&self.config.rustc_path);

        let out_dir = self.output_base_name().with_extension("pretty-out");
        let _ = fs::remove_dir_all(&out_dir);
        create_dir_all(&out_dir).unwrap();

        let target = if self.props.force_host {
            &*self.config.host
        } else {
            &*self.config.target
        };

        let aux_dir = self.aux_output_dir_name();

        rustc.arg("-")
            .arg("-Zno-trans")
            .arg("--out-dir").arg(&out_dir)
            .arg(&format!("--target={}", target))
            .arg("-L").arg(&self.config.build_base)
            .arg("-L").arg(aux_dir);

        if let Some(revision) = self.revision {
            rustc.args(&["--cfg", revision]);
        }

        rustc.args(self.split_maybe_args(&self.config.target_rustcflags));
        rustc.args(&self.props.compile_flags);

        self.compose_and_run_compiler(rustc, Some(src))
    }

    fn check_error_patterns(&self,
                            output_to_check: &str,
                            proc_res: &ProcRes) {
        if self.props.error_patterns.is_empty() {
            if self.props.must_compile_successfully {
                return
            } else {
                self.fatal(&format!("no error pattern specified in {:?}",
                                    self.testpaths.file.display()));
            }
        }
        let mut next_err_idx = 0;
        let mut next_err_pat = self.props.error_patterns[next_err_idx].trim();
        let mut done = false;
        for line in output_to_check.lines() {
            if line.contains(next_err_pat) {
                debug!("found error pattern {}", next_err_pat);
                next_err_idx += 1;
                if next_err_idx == self.props.error_patterns.len() {
                    debug!("found all error patterns");
                    done = true;
                    break;
                }
                next_err_pat = self.props.error_patterns[next_err_idx].trim();
            }
        }
        if done { return; }

        let missing_patterns = &self.props.error_patterns[next_err_idx..];
        if missing_patterns.len() == 1 {
            self.fatal_proc_rec(
                &format!("error pattern '{}' not found!", missing_patterns[0]),
                proc_res);
        } else {
            for pattern in missing_patterns {
                self.error(&format!("error pattern '{}' not found!", *pattern));
            }
            self.fatal_proc_rec("multiple error patterns not found", proc_res);
        }
    }

    fn check_no_compiler_crash(&self, proc_res: &ProcRes) {
        for line in proc_res.stderr.lines() {
            if line.contains("error: internal compiler error") {
                self.fatal_proc_rec("compiler encountered internal error", proc_res);
            }
        }
    }

    fn check_forbid_output(&self,
                           output_to_check: &str,
                           proc_res: &ProcRes) {
        for pat in &self.props.forbid_output {
            if output_to_check.contains(pat) {
                self.fatal_proc_rec("forbidden pattern found in compiler output", proc_res);
            }
        }
    }

    fn check_expected_errors(&self,
                             expected_errors: Vec<errors::Error>,
                             proc_res: &ProcRes) {
        if proc_res.status.success() &&
            expected_errors.iter().any(|x| x.kind == Some(ErrorKind::Error)) {
            self.fatal_proc_rec("process did not return an error status", proc_res);
        }

        let file_name =
            format!("{}", self.testpaths.file.display())
            .replace(r"\", "/"); // on windows, translate all '\' path separators to '/'

        // If the testcase being checked contains at least one expected "help"
        // message, then we'll ensure that all "help" messages are expected.
        // Otherwise, all "help" messages reported by the compiler will be ignored.
        // This logic also applies to "note" messages.
        let expect_help = expected_errors.iter().any(|ee| ee.kind == Some(ErrorKind::Help));
        let expect_note = expected_errors.iter().any(|ee| ee.kind == Some(ErrorKind::Note));

        // Parse the JSON output from the compiler and extract out the messages.
        let actual_errors = json::parse_output(&file_name, &proc_res.stderr, proc_res);
        let mut unexpected = Vec::new();
        let mut found = vec![false; expected_errors.len()];
        for actual_error in &actual_errors {
            let opt_index =
                expected_errors
                .iter()
                .enumerate()
                .position(|(index, expected_error)| {
                    !found[index] &&
                        actual_error.line_num == expected_error.line_num &&
                        (expected_error.kind.is_none() ||
                         actual_error.kind == expected_error.kind) &&
                        actual_error.msg.contains(&expected_error.msg)
                });

            match opt_index {
                Some(index) => {
                    // found a match, everybody is happy
                    assert!(!found[index]);
                    found[index] = true;
                }

                None => {
                    if self.is_unexpected_compiler_message(actual_error, expect_help, expect_note) {
                        self.error(
                            &format!("{}:{}: unexpected {}: '{}'",
                                     file_name,
                                     actual_error.line_num,
                                     actual_error.kind.as_ref()
                                     .map_or(String::from("message"),
                                             |k| k.to_string()),
                                     actual_error.msg));
                        unexpected.push(actual_error);
                    }
                }
            }
        }

        let mut not_found = Vec::new();
        // anything not yet found is a problem
        for (index, expected_error) in expected_errors.iter().enumerate() {
            if !found[index] {
                self.error(
                    &format!("{}:{}: expected {} not found: {}",
                             file_name,
                             expected_error.line_num,
                             expected_error.kind.as_ref()
                             .map_or("message".into(),
                                     |k| k.to_string()),
                             expected_error.msg));
                not_found.push(expected_error);
            }
        }

        if !unexpected.is_empty() || !not_found.is_empty() {
            self.error(
                &format!("{} unexpected errors found, {} expected errors not found",
                         unexpected.len(), not_found.len()));
            println!("status: {}\ncommand: {}",
                   proc_res.status, proc_res.cmdline);
            if !unexpected.is_empty() {
                println!("unexpected errors (from JSON output): {:#?}\n", unexpected);
            }
            if !not_found.is_empty() {
                println!("not found errors (from test file): {:#?}\n", not_found);
            }
            panic!();
        }
    }

    /// Returns true if we should report an error about `actual_error`,
    /// which did not match any of the expected error. We always require
    /// errors/warnings to be explicitly listed, but only require
    /// helps/notes if there are explicit helps/notes given.
    fn is_unexpected_compiler_message(&self,
                                      actual_error: &Error,
                                      expect_help: bool,
                                      expect_note: bool)
                                      -> bool {
        match actual_error.kind {
            Some(ErrorKind::Help) => expect_help,
            Some(ErrorKind::Note) => expect_note,
            Some(ErrorKind::Error) |
            Some(ErrorKind::Warning) => true,
            Some(ErrorKind::Suggestion) |
            None => false
        }
    }

    fn compile_test(&self) -> ProcRes {
        let mut rustc = self.make_compile_args(
            &self.testpaths.file, TargetLocation::ThisFile(self.make_exe_name()));

        rustc.arg("-L").arg(&self.aux_output_dir_name());

        match self.config.mode {
            CompileFail | Ui => {
                // compile-fail and ui tests tend to have tons of unused code as
                // it's just testing various pieces of the compile, but we don't
                // want to actually assert warnings about all this code. Instead
                // let's just ignore unused code warnings by defaults and tests
                // can turn it back on if needed.
                rustc.args(&["-A", "unused"]);
            }
            _ => {}
        }

        self.compose_and_run_compiler(rustc, None)
    }

    fn exec_compiled_test(&self) -> ProcRes {
        let env = &self.props.exec_env;

        match &*self.config.target {
            // This is pretty similar to below, we're transforming:
            //
            //      program arg1 arg2
            //
            // into
            //
            //      remote-test-client run program:support-lib.so arg1 arg2
            //
            // The test-client program will upload `program` to the emulator
            // along with all other support libraries listed (in this case
            // `support-lib.so`. It will then execute the program on the
            // emulator with the arguments specified (in the environment we give
            // the process) and then report back the same result.
            _ if self.config.remote_test_client.is_some() => {
                let aux_dir = self.aux_output_dir_name();
                let ProcArgs { mut prog, args } = self.make_run_args();
                if let Ok(entries) = aux_dir.read_dir() {
                    for entry in entries {
                        let entry = entry.unwrap();
                        if !entry.path().is_file() {
                            continue
                        }
                        prog.push_str(":");
                        prog.push_str(entry.path().to_str().unwrap());
                    }
                }
                let mut test_client = Command::new(
                    self.config.remote_test_client.as_ref().unwrap());
                test_client
                    .args(&["run", &prog])
                    .args(args)
                    .envs(env.clone());
                self.compose_and_run(test_client,
                                     self.config.run_lib_path.to_str().unwrap(),
                                     Some(aux_dir.to_str().unwrap()),
                                     None)
            }
            _ => {
                let aux_dir = self.aux_output_dir_name();
                let ProcArgs { prog, args } = self.make_run_args();
                let mut program = Command::new(&prog);
                program.args(args)
                    .current_dir(&self.output_base_name().parent().unwrap())
                    .envs(env.clone());
                self.compose_and_run(program,
                                     self.config.run_lib_path.to_str().unwrap(),
                                     Some(aux_dir.to_str().unwrap()),
                                     None)
            }
        }
    }

    /// For each `aux-build: foo/bar` annotation, we check to find the
    /// file in a `aux` directory relative to the test itself.
    fn compute_aux_test_paths(&self, rel_ab: &str) -> TestPaths {
        let test_ab = self.testpaths.file
                                    .parent()
                                    .expect("test file path has no parent")
                                    .join("auxiliary")
                                    .join(rel_ab);
        if !test_ab.exists() {
            self.fatal(&format!("aux-build `{}` source not found", test_ab.display()))
        }

        TestPaths {
            file: test_ab,
            base: self.testpaths.base.clone(),
            relative_dir: self.testpaths.relative_dir
                                        .join("auxiliary")
                                        .join(rel_ab)
                                        .parent()
                                        .expect("aux-build path has no parent")
                                        .to_path_buf()
        }
    }

    fn compose_and_run_compiler(&self, mut rustc: Command, input: Option<String>) -> ProcRes {
        if !self.props.aux_builds.is_empty() {
            create_dir_all(&self.aux_output_dir_name()).unwrap();
        }

        let aux_dir = self.aux_output_dir_name();

        for rel_ab in &self.props.aux_builds {
            let aux_testpaths = self.compute_aux_test_paths(rel_ab);
            let aux_props = self.props.from_aux_file(&aux_testpaths.file,
                                                     self.revision,
                                                     self.config);
            let aux_output = {
                let f = self.make_lib_name(&self.testpaths.file);
                let parent = f.parent().unwrap();
                TargetLocation::ThisDirectory(parent.to_path_buf())
            };
            let aux_cx = TestCx {
                config: self.config,
                props: &aux_props,
                testpaths: &aux_testpaths,
                revision: self.revision
            };
            let mut aux_rustc = aux_cx.make_compile_args(&aux_testpaths.file, aux_output);

            let crate_type = if aux_props.no_prefer_dynamic {
                None
            } else if (self.config.target.contains("musl") && !aux_props.force_host) ||
                      self.config.target.contains("wasm32") ||
                      self.config.target.contains("emscripten") {
                // We primarily compile all auxiliary libraries as dynamic libraries
                // to avoid code size bloat and large binaries as much as possible
                // for the test suite (otherwise including libstd statically in all
                // executables takes up quite a bit of space).
                //
                // For targets like MUSL or Emscripten, however, there is no support for
                // dynamic libraries so we just go back to building a normal library. Note,
                // however, that for MUSL if the library is built with `force_host` then
                // it's ok to be a dylib as the host should always support dylibs.
                Some("lib")
            } else {
                Some("dylib")
            };

            if let Some(crate_type) = crate_type {
                aux_rustc.args(&["--crate-type", crate_type]);
            }

            aux_rustc.arg("-L").arg(&aux_dir);

            let auxres = aux_cx.compose_and_run(aux_rustc,
                                                aux_cx.config.compile_lib_path.to_str().unwrap(),
                                                Some(aux_dir.to_str().unwrap()),
                                                None);
            if !auxres.status.success() {
                self.fatal_proc_rec(
                    &format!("auxiliary build of {:?} failed to compile: ",
                             aux_testpaths.file.display()),
                    &auxres);
            }
        }

        rustc.envs(self.props.rustc_env.clone());
        self.compose_and_run(rustc,
                             self.config.compile_lib_path.to_str().unwrap(),
                             Some(aux_dir.to_str().unwrap()),
                             input)
    }

    fn compose_and_run(&self,
                       mut command: Command,
                       lib_path: &str,
                       aux_path: Option<&str>,
                       input: Option<String>) -> ProcRes {
        let cmdline =
        {
            let cmdline = self.make_cmdline(&command, lib_path);
            logv(self.config, format!("executing {}", cmdline));
            cmdline
        };

        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped());

        // Need to be sure to put both the lib_path and the aux path in the dylib
        // search path for the child.
        let mut path = env::split_paths(&env::var_os(dylib_env_var()).unwrap_or(OsString::new()))
            .collect::<Vec<_>>();
        if let Some(p) = aux_path {
            path.insert(0, PathBuf::from(p))
        }
        path.insert(0, PathBuf::from(lib_path));

        // Add the new dylib search path var
        let newpath = env::join_paths(&path).unwrap();
        command.env(dylib_env_var(), newpath);

        let mut child = command.spawn().expect(&format!("failed to exec `{:?}`", &command));
        if let Some(input) = input {
            child.stdin.as_mut().unwrap().write_all(input.as_bytes()).unwrap();
        }

        let Output { status, stdout, stderr } = read2_abbreviated(child)
            .expect("failed to read output");

        let result = ProcRes {
            status,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            cmdline,
        };

        self.dump_output(&result.stdout, &result.stderr);

        result
    }

    fn make_compile_args(&self, input_file: &Path, output_file: TargetLocation) -> Command {
        let mut rustc = Command::new(&self.config.rustc_path);
        rustc.arg(input_file)
            .arg("-L").arg(&self.config.build_base);

        // Optionally prevent default --target if specified in test compile-flags.
        let custom_target = self.props.compile_flags
            .iter()
            .fold(false, |acc, x| acc || x.starts_with("--target"));

        if !custom_target {
            let target = if self.props.force_host {
                &*self.config.host
            } else {
                &*self.config.target
            };

            rustc.arg(&format!("--target={}", target));
        }

        if let Some(revision) = self.revision {
            rustc.args(&["--cfg", revision]);
        }

        if let Some(ref incremental_dir) = self.props.incremental_dir {
            rustc.args(&["-Z", &format!("incremental={}", incremental_dir.display())]);
            rustc.args(&["-Z", "incremental-verify-ich"]);
            rustc.args(&["-Z", "incremental-queries"]);
        }

        match self.config.mode {
            CompileFail => {
                // If we are extracting and matching errors in the new
                // fashion, then you want JSON mode. Old-skool error
                // patterns still match the raw compiler output.
                if self.props.error_patterns.is_empty() {
                    rustc.args(&["--error-format", "json"]);
                }
            }
            RunPass |
            RunFail |
            Pretty |
            RunMake |
            Ui => {
                // do not use JSON output
            }
        }


        if self.config.target == "wasm32-unknown-unknown" {
            // rustc.arg("-g"); // get any backtrace at all on errors
        } else if !self.props.no_prefer_dynamic {
            rustc.args(&["-C", "prefer-dynamic"]);
        }

        match output_file {
            TargetLocation::ThisFile(path) => {
                rustc.arg("-o").arg(path);
            }
            TargetLocation::ThisDirectory(path) => {
                rustc.arg("--out-dir").arg(path);
            }
        }

        if self.props.force_host {
            rustc.args(self.split_maybe_args(&self.config.host_rustcflags));
        } else {
            rustc.args(self.split_maybe_args(&self.config.target_rustcflags));
        }
        if let Some(ref linker) = self.config.linker {
            rustc.arg(format!("-Clinker={}", linker));
        }

        rustc.args(&self.props.compile_flags);

        rustc
    }

    fn make_lib_name(&self, auxfile: &Path) -> PathBuf {
        // what we return here is not particularly important, as it
        // happens; rustc ignores everything except for the directory.
        let auxname = self.output_testname(auxfile);
        self.aux_output_dir_name().join(&auxname)
    }

    fn make_exe_name(&self) -> PathBuf {
        let mut f = self.output_base_name();
        // FIXME: This is using the host architecture exe suffix, not target!
        if self.config.target.contains("emscripten") {
            let mut fname = f.file_name().unwrap().to_os_string();
            fname.push(".js");
            f.set_file_name(&fname);
        } else if self.config.target.contains("wasm32") {
            let mut fname = f.file_name().unwrap().to_os_string();
            fname.push(".wasm");
            f.set_file_name(&fname);
        } else if !env::consts::EXE_SUFFIX.is_empty() {
            let mut fname = f.file_name().unwrap().to_os_string();
            fname.push(env::consts::EXE_SUFFIX);
            f.set_file_name(&fname);
        }
        f
    }

    fn make_run_args(&self) -> ProcArgs {
        // If we've got another tool to run under (valgrind),
        // then split apart its command
        let mut args = self.split_maybe_args(&self.config.runtool);

        // If this is emscripten, then run tests under nodejs
        if self.config.target.contains("emscripten") {
            if let Some(ref p) = self.config.nodejs {
                args.push(p.clone());
            } else {
                self.fatal("no NodeJS binary found (--nodejs)");
            }
        }

        // If this is otherwise wasm , then run tests under nodejs with our
        // shim
        if self.config.target.contains("wasm32") {
            if let Some(ref p) = self.config.nodejs {
                args.push(p.clone());
            } else {
                self.fatal("no NodeJS binary found (--nodejs)");
            }

            let src = self.config.src_base
                .parent().unwrap() // chop off `run-pass`
                .parent().unwrap() // chop off `test`
                .parent().unwrap(); // chop off `src`
            args.push(src.join("src/etc/wasm32-shim.js").display().to_string());
        }

        let exe_file = self.make_exe_name();

        // FIXME (#9639): This needs to handle non-utf8 paths
        args.push(exe_file.to_str().unwrap().to_owned());

        // Add the arguments in the run_flags directive
        args.extend(self.split_maybe_args(&self.props.run_flags));

        let prog = args.remove(0);
         ProcArgs {
            prog,
            args,
        }
    }

    fn split_maybe_args(&self, argstr: &Option<String>) -> Vec<String> {
        match *argstr {
            Some(ref s) => {
                s
                    .split(' ')
                    .filter_map(|s| {
                        if s.chars().all(|c| c.is_whitespace()) {
                            None
                        } else {
                            Some(s.to_owned())
                        }
                    }).collect()
            }
            None => Vec::new()
        }
    }

    fn make_cmdline(&self, command: &Command, libpath: &str) -> String {
        use util;

        // Linux and mac don't require adjusting the library search path
        if cfg!(unix) {
            format!("{:?}", command)
        } else {
            // Build the LD_LIBRARY_PATH variable as it would be seen on the command line
            // for diagnostic purposes
            fn lib_path_cmd_prefix(path: &str) -> String {
                format!("{}=\"{}\"", util::lib_path_env_var(), util::make_new_path(path))
            }

            format!("{} {:?}", lib_path_cmd_prefix(libpath), command)
        }
    }

    fn dump_output(&self, out: &str, err: &str) {
        let revision = if let Some(r) = self.revision {
            format!("{}.", r)
        } else {
            String::new()
        };

        self.dump_output_file(out, &format!("{}out", revision));
        self.dump_output_file(err, &format!("{}err", revision));
        self.maybe_dump_to_stdout(out, err);
    }

    fn dump_output_file(&self,
                        out: &str,
                        extension: &str) {
        let outfile = self.make_out_name(extension);
        File::create(&outfile).unwrap().write_all(out.as_bytes()).unwrap();
    }

    fn make_out_name(&self, extension: &str) -> PathBuf {
        self.output_base_name().with_extension(extension)
    }

    fn aux_output_dir_name(&self) -> PathBuf {
        let f = self.output_base_name();
        let mut fname = f.file_name().unwrap().to_os_string();
        fname.push(&format!("{}.aux", self.config.mode.disambiguator()));
        f.with_file_name(&fname)
    }

    fn output_testname(&self, filepath: &Path) -> PathBuf {
        PathBuf::from(filepath.file_stem().unwrap())
    }

    /// Given a test path like `compile-fail/foo/bar.rs` Returns a name like
    /// `<output>/foo/bar-stage1`
    fn output_base_name(&self) -> PathBuf {
        let dir = self.config.build_base.join(&self.testpaths.relative_dir);

        // Note: The directory `dir` is created during `collect_tests_from_dir`
        dir
            .join(&self.output_testname(&self.testpaths.file))
            .with_extension(&self.config.stage_id)
    }

    fn maybe_dump_to_stdout(&self, out: &str, err: &str) {
        if self.config.verbose {
            println!("------{}------------------------------", "stdout");
            println!("{}", out);
            println!("------{}------------------------------", "stderr");
            println!("{}", err);
            println!("------------------------------------------");
        }
    }

    fn error(&self, err: &str) {
        match self.revision {
            Some(rev) => println!("\nerror in revision `{}`: {}", rev, err),
            None => println!("\nerror: {}", err)
        }
    }

    fn fatal(&self, err: &str) -> ! {
        self.error(err); panic!();
    }

    fn fatal_proc_rec(&self, err: &str, proc_res: &ProcRes) -> ! {
        self.error(err);
        proc_res.fatal(None);
    }

    // codegen tests (using FileCheck)

    fn run_rmake_test(&self) {
        // FIXME(#11094): we should fix these tests
        if self.config.host != self.config.target {
            return
        }

        let cwd = env::current_dir().unwrap();
        let src_root = self.config.src_base.parent().unwrap()
                                           .parent().unwrap()
                                           .parent().unwrap();
        let src_root = cwd.join(&src_root);

        let tmpdir = cwd.join(self.output_base_name());
        if tmpdir.exists() {
            self.aggressive_rm_rf(&tmpdir).unwrap();
        }
        create_dir_all(&tmpdir).unwrap();

        let host = &self.config.host;
        let make = if host.contains("bitrig") || host.contains("dragonfly") ||
            host.contains("freebsd") || host.contains("netbsd") ||
            host.contains("openbsd") {
            "gmake"
        } else {
            "make"
        };

        let mut cmd = Command::new(make);
        cmd.current_dir(&self.testpaths.file)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped())
           .env("TARGET", &self.config.target)
           .env("PYTHON", &self.config.docck_python)
           .env("S", src_root)
           .env("RUST_BUILD_STAGE", &self.config.stage_id)
           .env("RUSTC", cwd.join(&self.config.rustc_path))
           .env("RUSTDOC",
               cwd.join(&self.config.rustdoc_path.as_ref().expect("--rustdoc-path passed")))
           .env("TMPDIR", &tmpdir)
           .env("LD_LIB_PATH_ENVVAR", dylib_env_var())
           .env("HOST_RPATH_DIR", cwd.join(&self.config.compile_lib_path))
           .env("TARGET_RPATH_DIR", cwd.join(&self.config.run_lib_path))
           .env("LLVM_COMPONENTS", &self.config.llvm_components)
           .env("LLVM_CXXFLAGS", &self.config.llvm_cxxflags);

        if let Some(ref linker) = self.config.linker {
            cmd.env("RUSTC_LINKER", linker);
        }

        // We don't want RUSTFLAGS set from the outside to interfere with
        // compiler flags set in the test cases:
        cmd.env_remove("RUSTFLAGS");

        if self.config.target.contains("msvc") {
            // We need to pass a path to `lib.exe`, so assume that `cc` is `cl.exe`
            // and that `lib.exe` lives next to it.
            let lib = Path::new(&self.config.cc).parent().unwrap().join("lib.exe");

            // MSYS doesn't like passing flags of the form `/foo` as it thinks it's
            // a path and instead passes `C:\msys64\foo`, so convert all
            // `/`-arguments to MSVC here to `-` arguments.
            let cflags = self.config.cflags.split(' ').map(|s| s.replace("/", "-"))
                                                 .collect::<Vec<_>>().join(" ");

            cmd.env("IS_MSVC", "1")
               .env("IS_WINDOWS", "1")
               .env("MSVC_LIB", format!("'{}' -nologo", lib.display()))
               .env("CC", format!("'{}' {}", self.config.cc, cflags))
               .env("CXX", &self.config.cxx);
        } else {
            cmd.env("CC", format!("{} {}", self.config.cc, self.config.cflags))
               .env("CXX", format!("{} {}", self.config.cxx, self.config.cflags))
               .env("AR", &self.config.ar);

            if self.config.target.contains("windows") {
                cmd.env("IS_WINDOWS", "1");
            }
        }

        let output = cmd.spawn().and_then(read2_abbreviated).expect("failed to spawn `make`");
        if !output.status.success() {
            let res = ProcRes {
                status: output.status,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                cmdline: format!("{:?}", cmd),
            };
            self.fatal_proc_rec("make failed", &res);
        }
    }

    fn aggressive_rm_rf(&self, path: &Path) -> io::Result<()> {
        for e in path.read_dir()? {
            let entry = e?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                self.aggressive_rm_rf(&path)?;
            } else {
                // Remove readonly files as well on windows (by default we can't)
                fs::remove_file(&path).or_else(|e| {
                    if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied {
                        let mut meta = entry.metadata()?.permissions();
                        meta.set_readonly(false);
                        fs::set_permissions(&path, meta)?;
                        fs::remove_file(&path)
                    } else {
                        Err(e)
                    }
                })?;
            }
        }
        fs::remove_dir(path)
    }

    fn run_ui_test(&self) {
        let proc_res = self.compile_test();

        let expected_stderr_path = self.expected_output_path("stderr");
        let expected_stderr = self.load_expected_output(&expected_stderr_path);

        let expected_stdout_path = self.expected_output_path("stdout");
        let expected_stdout = self.load_expected_output(&expected_stdout_path);

        let normalized_stdout =
            self.normalize_output(&proc_res.stdout, &self.props.normalize_stdout);
        let normalized_stderr =
            self.normalize_output(&proc_res.stderr, &self.props.normalize_stderr);

        let mut errors = 0;
        errors += self.compare_output("stdout", &normalized_stdout, &expected_stdout);
        errors += self.compare_output("stderr", &normalized_stderr, &expected_stderr);

        if errors > 0 {
            println!("To update references, run this command from build directory:");
            let relative_path_to_file =
                self.testpaths.relative_dir
                              .join(self.testpaths.file.file_name().unwrap());
            println!("{}/update-references.sh '{}' '{}'",
                     self.config.src_base.display(),
                     self.config.build_base.display(),
                     relative_path_to_file.display());
            self.fatal_proc_rec(&format!("{} errors occurred comparing output.", errors),
                                &proc_res);
        }

        if self.props.run_pass {
            let proc_res = self.exec_compiled_test();

            if !proc_res.status.success() {
                self.fatal_proc_rec("test run failed!", &proc_res);
            }
        }
    }

    fn normalize_output(&self, output: &str, custom_rules: &[(String, String)]) -> String {
        let parent_dir = self.testpaths.file.parent().unwrap();
        let cflags = self.props.compile_flags.join(" ");
        let json = cflags.contains("--error-format json") ||
                   cflags.contains("--error-format pretty-json");
        let parent_dir_str = if json {
            parent_dir.display().to_string().replace("\\", "\\\\")
        } else {
            parent_dir.display().to_string()
        };

        let mut normalized = output.replace(&parent_dir_str, "$DIR");

        if json {
            // escaped newlines in json strings should be readable
            // in the stderr files. There's no point int being correct,
            // since only humans process the stderr files.
            // Thus we just turn escaped newlines back into newlines.
            normalized = normalized.replace("\\n", "\n");
        }

        normalized = normalized.replace("\\\\", "\\") // denormalize for paths on windows
              .replace("\\", "/") // normalize for paths on windows
              .replace("\r\n", "\n") // normalize for linebreaks on windows
              .replace("\t", "\\t"); // makes tabs visible
        for rule in custom_rules {
            normalized = normalized.replace(&rule.0, &rule.1);
        }
        normalized
    }

    fn expected_output_path(&self, kind: &str) -> PathBuf {
        let extension = match self.revision {
            Some(r) => format!("{}.{}", r, kind),
            None => kind.to_string(),
        };
        self.testpaths.file.with_extension(extension)
    }

    fn load_expected_output(&self, path: &Path) -> String {
        if !path.exists() {
            return String::new();
        }

        let mut result = String::new();
        match File::open(path).and_then(|mut f| f.read_to_string(&mut result)) {
            Ok(_) => result,
            Err(e) => {
                self.fatal(&format!("failed to load expected output from `{}`: {}",
                                    path.display(), e))
            }
        }
    }

    fn compare_output(&self, kind: &str, actual: &str, expected: &str) -> usize {
        if actual == expected {
            return 0;
        }

        println!("normalized {}:\n{}\n", kind, actual);
        println!("expected {}:\n{}\n", kind, expected);
        println!("diff of {}:\n", kind);

        for diff in diff::lines(expected, actual) {
            match diff {
                diff::Result::Left(l)    => println!("-{}", l),
                diff::Result::Both(l, _) => println!(" {}", l),
                diff::Result::Right(r)   => println!("+{}", r),
            }
        }

        let output_file = self.output_base_name().with_extension(kind);
        match File::create(&output_file).and_then(|mut f| f.write_all(actual.as_bytes())) {
            Ok(()) => { }
            Err(e) => {
                self.fatal(&format!("failed to write {} to `{}`: {}",
                                    kind, output_file.display(), e))
            }
        }

        println!("\nThe actual {0} differed from the expected {0}.", kind);
        println!("Actual {} saved to {}", kind, output_file.display());
        1
    }
}

struct ProcArgs {
    prog: String,
    args: Vec<String>,
}

pub struct ProcRes {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    cmdline: String,
}

impl ProcRes {
    pub fn fatal(&self, err: Option<&str>) -> ! {
        if let Some(e) = err {
            println!("\nerror: {}", e);
        }
        print!("\
            status: {}\n\
            command: {}\n\
            stdout:\n\
            ------------------------------------------\n\
            {}\n\
            ------------------------------------------\n\
            stderr:\n\
            ------------------------------------------\n\
            {}\n\
            ------------------------------------------\n\
            \n",
               self.status, self.cmdline, self.stdout,
               self.stderr);
        panic!();
    }
}

enum TargetLocation {
    ThisFile(PathBuf),
    ThisDirectory(PathBuf),
}

fn read2_abbreviated(mut child: Child) -> io::Result<Output> {
    use std::mem::replace;
    use read2::read2;

    const HEAD_LEN: usize = 160 * 1024;
    const TAIL_LEN: usize = 256 * 1024;

    enum ProcOutput {
        Full(Vec<u8>),
        Abbreviated {
            head: Vec<u8>,
            skipped: usize,
            tail: Box<[u8]>,
        }
    }

    impl ProcOutput {
        fn extend(&mut self, data: &[u8]) {
            let new_self = match *self {
                ProcOutput::Full(ref mut bytes) => {
                    bytes.extend_from_slice(data);
                    let new_len = bytes.len();
                    if new_len <= HEAD_LEN + TAIL_LEN {
                        return;
                    }
                    let tail = bytes.split_off(new_len - TAIL_LEN).into_boxed_slice();
                    let head = replace(bytes, Vec::new());
                    let skipped = new_len - HEAD_LEN - TAIL_LEN;
                    ProcOutput::Abbreviated { head, skipped, tail }
                }
                ProcOutput::Abbreviated { ref mut skipped, ref mut tail, .. } => {
                    *skipped += data.len();
                    if data.len() <= TAIL_LEN {
                        tail[..data.len()].copy_from_slice(data);
                        #[cfg(not(feature = "stable"))]
                        tail.rotate_left(data.len());
                        // FIXME: Remove this when rotate_left is stable in 1.26
                        #[cfg(feature = "stable")]
                        rotate_left(tail, data.len());
                    } else {
                        tail.copy_from_slice(&data[(data.len() - TAIL_LEN)..]);
                    }
                    return;
                }
            };
            *self = new_self;
        }

        fn into_bytes(self) -> Vec<u8> {
            match self {
                ProcOutput::Full(bytes) => bytes,
                ProcOutput::Abbreviated { mut head, skipped, tail } => {
                    write!(&mut head, "\n\n<<<<<< SKIPPED {} BYTES >>>>>>\n\n", skipped).unwrap();
                    head.extend_from_slice(&tail);
                    head
                }
            }
        }
    }

    let mut stdout = ProcOutput::Full(Vec::new());
    let mut stderr = ProcOutput::Full(Vec::new());

    drop(child.stdin.take());
    read2(child.stdout.take().unwrap(), child.stderr.take().unwrap(), &mut |is_stdout, data, _| {
        if is_stdout { &mut stdout } else { &mut stderr }.extend(data);
        data.clear();
    })?;
    let status = child.wait()?;

    Ok(Output {
        status,
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
    })
}

// FIXME: Remove this when rotate_left is stable in 1.26
#[cfg(feature = "stable")]
fn rotate_left<T>(slice: &mut [T], places: usize) {
    // Rotation can be implemented by reversing the slice,
    // splitting the slice in two, and then reversing the
    // two sub-slices.
    slice.reverse();
    let (a, b) = slice.split_at_mut(places);
    a.reverse();
    b.reverse();
}
