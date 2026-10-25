// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//! Unit tests for the CLI flag-parsing helper (`cli::Args`), issue #556.
//!
//! `cli_tests` covers each subcommand in depth; this module pins the helper's
//! contract for operator-facing flags: accepted spellings parse into the
//! expected typed values, and malformed flags fail with the specific clap
//! error kind instead of being silently ignored.

#[cfg(test)]
mod tests {
    use crate::cli::{Args, Commands};
    use clap::error::ErrorKind;
    use clap::Parser;

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        let mut full = vec!["stellar-operator"];
        full.extend_from_slice(argv);
        Args::try_parse_from(full)
    }

    fn run_args(args: &[&str]) -> crate::cli::RunArgs {
        match parse(args).expect("run flags should parse").command {
            Commands::Run(run) => run,
            other => panic!("expected the run subcommand, got {other:?}"),
        }
    }

    #[test]
    fn valid_run_flags_are_parsed_into_typed_values() {
        let run = run_args(&[
            "run",
            "--namespace",
            "stellar-system",
            "--log-level",
            "debug",
            "--enable-mtls",
            "--dry-run",
        ]);

        assert_eq!(run.namespace, "stellar-system");
        assert_eq!(run.log_level, "debug");
        assert!(run.enable_mtls);
        assert!(run.dry_run);
        // Omitted flags keep their documented defaults.
        assert!(!run.scheduler);
        assert_eq!(run.scheduler_name, "stellar-scheduler");
    }

    #[test]
    fn global_flag_is_accepted_before_and_after_the_subcommand() {
        for argv in [vec!["--offline", "info"], vec!["info", "--offline"]] {
            let parsed = parse(&argv).expect("global --offline should be accepted");
            assert!(parsed.offline, "argv: {argv:?}");
        }
    }

    #[test]
    fn unknown_flag_is_rejected() {
        let error = parse(&["run", "--not-a-real-flag"]).expect_err("unknown flag must fail");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn missing_flag_value_is_rejected() {
        let error = parse(&["run", "--namespace"]).expect_err("namespace needs a value");
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn invalid_enum_value_is_rejected() {
        let error =
            parse(&["webhook", "--log-format", "yaml"]).expect_err("yaml is not a log format");
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        let error = parse(&["frobnicate"]).expect_err("unknown subcommand must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn missing_subcommand_is_rejected() {
        let error = parse(&[]).expect_err("a subcommand is required");
        assert_eq!(error.kind(), ErrorKind::MissingSubcommand);
    }

    #[test]
    fn non_numeric_numeric_flag_is_rejected() {
        let error = parse(&["run", "--retry-secs", "soon"]).expect_err("retry-secs is a number");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }
}
