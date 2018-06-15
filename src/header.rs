// Copyright 2012-2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::env;
use std::fs::File;
use std::io::BufReader;
use std::io::prelude::*;
use std::path::{Path, PathBuf};

use common::Config;
use util;

/// Properties which must be known very early, before actually running
/// the test.
pub struct EarlyProps {
    pub ignore: bool,
    pub should_fail: bool,
    pub aux: Vec<String>,
}

impl EarlyProps {
    pub fn from_file(config: &Config, testfile: &Path) -> Self {
        let mut props = EarlyProps {
            ignore: false,
            should_fail: false,
            aux: Vec::new(),
        };

        iter_header(testfile,
                    None,
                    &mut |ln| {
            props.ignore =
                props.ignore ||
                config.parse_cfg_name_directive(ln, "ignore") ||
                ignore_llvm(config, ln);

            if let Some(s) = config.parse_aux_build(ln) {
                props.aux.push(s);
            }

            props.should_fail = props.should_fail || config.parse_name_directive(ln, "should-fail");
        });

        return props;

        fn ignore_llvm(config: &Config, line: &str) -> bool {
            if config.system_llvm && line.starts_with("no-system-llvm") {
                    return true;
            }
            if let Some(ref actual_version) = config.llvm_version {
                if line.starts_with("min-llvm-version") {
                    let min_version = line.trim_right()
                        .rsplit(' ')
                        .next()
                        .expect("Malformed llvm version directive");
                    // Ignore if actual version is smaller the minimum required
                    // version
                    &actual_version[..] < min_version
                } else if line.starts_with("min-system-llvm-version") {
                    let min_version = line.trim_right()
                        .rsplit(' ')
                        .next()
                        .expect("Malformed llvm version directive");
                    // Ignore if using system LLVM and actual version
                    // is smaller the minimum required version
                    !(config.system_llvm && &actual_version[..] < min_version)
                } else {
                    false
                }
            } else {
                false
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct TestProps {
    // Lines that should be expected, in order, on standard out
    pub error_patterns: Vec<String>,
    // Extra flags to pass to the compiler
    pub compile_flags: Vec<String>,
    // Extra flags to pass when the compiled code is run (such as --bench)
    pub run_flags: Option<String>,
    // Other crates that should be compiled (typically from the same
    // directory as the test, but for backwards compatibility reasons
    // we also check the auxiliary directory)
    pub aux_builds: Vec<String>,
    // Environment settings to use for compiling
    pub rustc_env: Vec<(String, String)>,
    // Environment settings to use during execution
    pub exec_env: Vec<(String, String)>,
    // Lines to check if they appear in the expected debugger output
    pub check_lines: Vec<String>,
    // Build documentation for all specified aux-builds as well
    pub build_aux_docs: bool,
    // Flag to force a crate to be built with the host architecture
    pub force_host: bool,
    // Check stdout for error-pattern output as well as stderr
    pub check_stdout: bool,
    // Don't force a --crate-type=dylib flag on the command line
    pub no_prefer_dynamic: bool,
    // Run --pretty expanded when running pretty printing tests
    pub pretty_expanded: bool,
    // Only compare pretty output and don't try compiling
    pub pretty_compare_only: bool,
    // Patterns which must not appear in the output of a cfail test.
    pub forbid_output: Vec<String>,
    // Revisions to test for incremental compilation.
    pub revisions: Vec<String>,
    // Specifies that a cfail test must actually compile without errors.
    pub must_compile_successfully: bool,
    // rustdoc will test the output of the `--test` option
    pub check_test_line_numbers_match: bool,
    // customized normalization rules
    pub normalize_stdout: Vec<(String, String)>,
    pub normalize_stderr: Vec<(String, String)>,
}

impl TestProps {
    pub fn new() -> Self {
        TestProps {
            error_patterns: vec![],
            compile_flags: vec![],
            run_flags: None,
            aux_builds: vec![],
            revisions: vec![],
            rustc_env: vec![],
            exec_env: vec![],
            check_lines: vec![],
            build_aux_docs: false,
            force_host: false,
            check_stdout: false,
            no_prefer_dynamic: false,
            pretty_expanded: false,
            pretty_compare_only: false,
            forbid_output: vec![],
            must_compile_successfully: false,
            check_test_line_numbers_match: false,
            normalize_stdout: vec![],
            normalize_stderr: vec![],
        }
    }

    pub fn from_aux_file(&self,
                         testfile: &Path,
                         cfg: Option<&str>,
                         config: &Config)
                         -> Self {
        let mut props = TestProps::new();

        // copy over select properties to the aux build:
        props.load_from(testfile, cfg, config);

        props
    }

    pub fn from_file(testfile: &Path, cfg: Option<&str>, config: &Config) -> Self {
        let mut props = TestProps::new();
        props.load_from(testfile, cfg, config);
        props
    }

    /// Load properties from `testfile` into `props`. If a property is
    /// tied to a particular revision `foo` (indicated by writing
    /// `//[foo]`), then the property is ignored unless `cfg` is
    /// `Some("foo")`.
    fn load_from(&mut self,
                 testfile: &Path,
                 cfg: Option<&str>,
                 config: &Config) {
        iter_header(testfile,
                    cfg,
                    &mut |ln| {
            if let Some(ep) = config.parse_error_pattern(ln) {
                self.error_patterns.push(ep);
            }

            if let Some(flags) = config.parse_compile_flags(ln) {
                self.compile_flags.extend(flags.split_whitespace()
                    .map(|s| s.to_owned()));
            }

            if let Some(r) = config.parse_revisions(ln) {
                self.revisions.extend(r);
            }

            if self.run_flags.is_none() {
                self.run_flags = config.parse_run_flags(ln);
            }

            if !self.build_aux_docs {
                self.build_aux_docs = config.parse_build_aux_docs(ln);
            }

            if !self.force_host {
                self.force_host = config.parse_force_host(ln);
            }

            if !self.check_stdout {
                self.check_stdout = config.parse_check_stdout(ln);
            }

            if !self.no_prefer_dynamic {
                self.no_prefer_dynamic = config.parse_no_prefer_dynamic(ln);
            }

            if !self.pretty_expanded {
                self.pretty_expanded = config.parse_pretty_expanded(ln);
            }

            if !self.pretty_compare_only {
                self.pretty_compare_only = config.parse_pretty_compare_only(ln);
            }

            if let Some(ab) = config.parse_aux_build(ln) {
                self.aux_builds.push(ab);
            }

            if let Some(ee) = config.parse_env(ln, "exec-env") {
                self.exec_env.push(ee);
            }

            if let Some(ee) = config.parse_env(ln, "rustc-env") {
                self.rustc_env.push(ee);
            }

            if let Some(cl) = config.parse_check_line(ln) {
                self.check_lines.push(cl);
            }

            if let Some(of) = config.parse_forbid_output(ln) {
                self.forbid_output.push(of);
            }

            if !self.must_compile_successfully {
                self.must_compile_successfully = config.parse_must_compile_successfully(ln);
            }

            if !self.check_test_line_numbers_match {
                self.check_test_line_numbers_match = config.parse_check_test_line_numbers_match(ln);
            }

            if let Some(rule) = config.parse_custom_normalization(ln, "normalize-stdout") {
                self.normalize_stdout.push(rule);
            }
            if let Some(rule) = config.parse_custom_normalization(ln, "normalize-stderr") {
                self.normalize_stderr.push(rule);
            }
        });

        for key in &["RUST_TEST_NOCAPTURE", "RUST_TEST_THREADS"] {
            if let Ok(val) = env::var(key) {
                if self.exec_env.iter().find(|&&(ref x, _)| x == key).is_none() {
                    self.exec_env.push(((*key).to_owned(), val))
                }
            }
        }
    }
}

fn iter_header(testfile: &Path, cfg: Option<&str>, it: &mut FnMut(&str)) {
    if testfile.is_dir() {
        return;
    }
    let rdr = BufReader::new(File::open(testfile).unwrap());
    for ln in rdr.lines() {
        // Assume that any directives will be found before the first
        // module or function. This doesn't seem to be an optimization
        // with a warm page cache. Maybe with a cold one.
        let ln = ln.unwrap();
        let ln = ln.trim();
        if ln.starts_with("fn") || ln.starts_with("mod") {
            return;
        } else if ln.starts_with("//[") {
            // A comment like `//[foo]` is specific to revision `foo`
            if let Some(close_brace) = ln.find(']') {
                let lncfg = &ln[3..close_brace];
                let matches = match cfg {
                    Some(s) => s == &lncfg[..],
                    None => false,
                };
                if matches {
                    it(ln[(close_brace + 1) ..].trim_left());
                }
            } else {
                panic!("malformed condition directive: expected `//[foo]`, found `{}`",
                       ln)
            }
        } else if ln.starts_with("//") {
            it(ln[2..].trim_left());
        }
    }
    return;
}

impl Config {
    fn parse_error_pattern(&self, line: &str) -> Option<String> {
        self.parse_name_value_directive(line, "error-pattern")
    }

    fn parse_forbid_output(&self, line: &str) -> Option<String> {
        self.parse_name_value_directive(line, "forbid-output")
    }

    fn parse_aux_build(&self, line: &str) -> Option<String> {
        self.parse_name_value_directive(line, "aux-build")
    }

    fn parse_compile_flags(&self, line: &str) -> Option<String> {
        self.parse_name_value_directive(line, "compile-flags")
    }

    fn parse_revisions(&self, line: &str) -> Option<Vec<String>> {
        self.parse_name_value_directive(line, "revisions")
            .map(|r| r.split_whitespace().map(|t| t.to_string()).collect())
    }

    fn parse_run_flags(&self, line: &str) -> Option<String> {
        self.parse_name_value_directive(line, "run-flags")
    }

    fn parse_check_line(&self, line: &str) -> Option<String> {
        self.parse_name_value_directive(line, "check")
    }

    fn parse_force_host(&self, line: &str) -> bool {
        self.parse_name_directive(line, "force-host")
    }

    fn parse_build_aux_docs(&self, line: &str) -> bool {
        self.parse_name_directive(line, "build-aux-docs")
    }

    fn parse_check_stdout(&self, line: &str) -> bool {
        self.parse_name_directive(line, "check-stdout")
    }

    fn parse_no_prefer_dynamic(&self, line: &str) -> bool {
        self.parse_name_directive(line, "no-prefer-dynamic")
    }

    fn parse_pretty_expanded(&self, line: &str) -> bool {
        self.parse_name_directive(line, "pretty-expanded")
    }

    fn parse_pretty_compare_only(&self, line: &str) -> bool {
        self.parse_name_directive(line, "pretty-compare-only")
    }

    fn parse_must_compile_successfully(&self, line: &str) -> bool {
        self.parse_name_directive(line, "must-compile-successfully")
    }

    fn parse_check_test_line_numbers_match(&self, line: &str) -> bool {
        self.parse_name_directive(line, "check-test-line-numbers-match")
    }

    fn parse_env(&self, line: &str, name: &str) -> Option<(String, String)> {
        self.parse_name_value_directive(line, name).map(|nv| {
            // nv is either FOO or FOO=BAR
            let mut strs: Vec<String> = nv.splitn(2, '=')
                .map(str::to_owned)
                .collect();

            match strs.len() {
                1 => (strs.pop().unwrap(), "".to_owned()),
                2 => {
                    let end = strs.pop().unwrap();
                    (strs.pop().unwrap(), end)
                }
                n => panic!("Expected 1 or 2 strings, not {}", n),
            }
        })
    }

    fn parse_custom_normalization(&self, mut line: &str, prefix: &str) -> Option<(String, String)> {
        if self.parse_cfg_name_directive(line, prefix) {
            let from = parse_normalization_string(&mut line)?;
            let to = parse_normalization_string(&mut line)?;
            Some((from, to))
        } else {
            None
        }
    }

    /// Parses a name-value directive which contains config-specific information, e.g. `ignore-x86`
    /// or `normalize-stderr-32bit`. Returns `true` if the line matches it.
    fn parse_cfg_name_directive(&self, line: &str, prefix: &str) -> bool {
        if line.starts_with(prefix) && line.as_bytes().get(prefix.len()) == Some(&b'-') {
            let name = line[prefix.len()+1 ..].split(&[':', ' '][..]).next().unwrap();

            name == "test" ||
                util::matches_os(&self.target, name) ||             // target
                name == util::get_arch(&self.target) ||             // architecture
                name == util::get_pointer_width(&self.target) ||    // pointer width
                name == self.stage_id.split('-').next().unwrap() || // stage
                Some(name) == util::get_env(&self.target) ||        // env
                (self.target != self.host && name == "cross-compile")
        } else {
            false
        }
    }

    fn parse_name_directive(&self, line: &str, directive: &str) -> bool {
        // Ensure the directive is a whole word. Do not match "ignore-x86" when
        // the line says "ignore-x86_64".
        line.starts_with(directive) && match line.as_bytes().get(directive.len()) {
            None | Some(&b' ') | Some(&b':') => true,
            _ => false
        }
    }

    pub fn parse_name_value_directive(&self, line: &str, directive: &str) -> Option<String> {
        let colon = directive.len();
        if line.starts_with(directive) && line.as_bytes().get(colon) == Some(&b':') {
            let value = line[(colon + 1) ..].to_owned();
            debug!("{}: {}", directive, value);
            Some(expand_variables(value, self))
        } else {
            None
        }
    }

    pub fn find_rust_src_root(&self) -> Option<PathBuf> {
        let mut path = self.src_base.clone();
        let path_postfix = Path::new("src/etc/lldb_batchmode.py");

        while path.pop() {
            if path.join(&path_postfix).is_file() {
                return Some(path);
            }
        }

        None
    }
}

pub fn lldb_version_to_int(version_string: &str) -> isize {
    let error_string = format!("Encountered LLDB version string with unexpected format: {}",
                               version_string);
    version_string.parse().expect(&error_string)
}

fn expand_variables(mut value: String, config: &Config) -> String {
    const CWD: &'static str = "{{cwd}}";
    const SRC_BASE: &'static str = "{{src-base}}";
    const BUILD_BASE: &'static str = "{{build-base}}";

    if value.contains(CWD) {
        let cwd = env::current_dir().unwrap();
        value = value.replace(CWD, &cwd.to_string_lossy());
    }

    if value.contains(SRC_BASE) {
        value = value.replace(SRC_BASE, &config.src_base.to_string_lossy());
    }

    if value.contains(BUILD_BASE) {
        value = value.replace(BUILD_BASE, &config.build_base.to_string_lossy());
    }

    value
}

/// Finds the next quoted string `"..."` in `line`, and extract the content from it. Move the `line`
/// variable after the end of the quoted string.
///
/// # Examples
///
/// ```ignore
/// let mut s = "normalize-stderr-32bit: \"something (32 bits)\" -> \"something ($WORD bits)\".";
/// let first = parse_normalization_string(&mut s);
/// assert_eq!(first, Some("something (32 bits)".to_owned()));
/// assert_eq!(s, " -> \"something ($WORD bits)\".");
/// ```
fn parse_normalization_string(line: &mut &str) -> Option<String> {
    // FIXME support escapes in strings.
    let begin = line.find('"')? + 1;
    let end = line[begin..].find('"')? + begin;
    let result = line[begin..end].to_owned();
    *line = &line[end+1..];
    Some(result)
}
