//! Private development runner behind `cargo xtask <verb>`.
//!
//! Tasks 1-2 implemented argument parsing plus the run-record core; all
//! seven verbs dispatch to `verbs.rs` runners (script verbs shell out to
//! `tools/`, suite verbs run scoped `cargo test` selectors).

mod run;
mod verbs;

#[derive(Debug, PartialEq)]
enum Command {
    CaptureLemma {
        source: String,
        dry_run: bool,
    },
    Capabilities {
        candidate: String,
        dry_run: bool,
    },
    Conformance {
        profile: String,
        dry_run: bool,
    },
    Recovery {
        suite: String,
        dry_run: bool,
    },
    Benchmark {
        manifest: String,
        limit: Option<u64>,
        dry_run: bool,
    },
    Quality {
        split: String,
        dry_run: bool,
    },
    Evidence {
        release: String,
        dry_run: bool,
    },
    Help {
        verb: Option<String>,
    },
}

const VERBS: &[&str] = &[
    "capture-lemma",
    "capabilities",
    "conformance",
    "recovery",
    "benchmark",
    "quality",
    "evidence",
];

const BOOL_FLAGS: &[&str] = &["--dry-run", "--help"];

fn usage() -> &'static str {
    "cargo xtask <verb> [flags] [--dry-run]\n\
     \n\
     Private development runner: delegates to existing tools/ scripts and test suites.\n\
     \n\
     Verbs:\n\
       capture-lemma --source <checkout>        capture a Lemma baseline into a sandbox\n\
       capabilities --candidate <name>          run backend-gate suites (only fjall-lance resolves)\n\
       conformance --profile <name>             run compat + differential suites (only lemma-0.21.0 resolves)\n\
       recovery --suite <name>                  run restore/kill/reopen suites (only durable resolves)\n\
       benchmark --manifest <file> [--limit N]  run the benchmark harness\n\
       quality --split <name>                   run calibration corpus + quality wave\n\
       evidence --release <version>             build the release evidence bundle\n\
     \n\
     Global flags:\n\
       --dry-run   print the would-be command without executing\n\
       --help, -h  show this help (or `cargo xtask <verb> --help` for one verb)\n\
       help [verb] show this help (or for one verb)\n\
     \n\
     Verbs and flags accept unambiguous prefixes (`_` also reads as `-`). Unknown verbs, flags,\n\
     candidates, profiles, splits, and suites are hard errors naming the token.\n\
     A repeated value flag is an error; `--` ends flag parsing (anything\n\
     after it is an unexpected argument).\n\
     \n\
     Examples:\n\
       cargo xtask --help\n\
       cargo xtask evidence --help\n\
       cargo xtask evidence --release v0.1 --dry-run\n"
}

fn canonical_verb(token: &str) -> Result<&'static str, String> {
    let normalized = token.replace('_', "-");
    if let Some(hit) = VERBS.iter().find(|v| v[..] == normalized[..]) {
        return Ok(*hit);
    }
    let hits: Vec<&'static str> = VERBS
        .iter()
        .filter(|v| v.starts_with(normalized.as_str()))
        .copied()
        .collect();
    match hits.as_slice() {
        [] => Err(format!("unknown verb '{token}'")),
        [single] => Ok(*single),
        _ => Err(format!(
            "ambiguous verb prefix '{token}' (matches: {})",
            hits.join(", ")
        )),
    }
}

fn known_value_flags(verb: &str) -> &'static [&'static str] {
    match verb {
        "capture-lemma" => &["--source"],
        "capabilities" => &["--candidate"],
        "conformance" => &["--profile"],
        "recovery" => &["--suite"],
        "benchmark" => &["--manifest", "--limit"],
        "quality" => &["--split"],
        "evidence" => &["--release"],
        _ => &[],
    }
}

fn flag_name(token: &str) -> String {
    let name = match token.find('=') {
        Some(i) => &token[..i],
        None => token,
    };
    name.replace('_', "-")
}

fn flag_value(token: &str) -> Option<&str> {
    match token.find('=') {
        Some(i) => Some(&token[i + 1..]),
        None => None,
    }
}

fn resolve_flag<'a>(name: &str, known: &[&'a str]) -> Result<&'a str, String> {
    if let Some(exact) = known.iter().find(|k| k[..] == name[..]) {
        return Ok(*exact);
    }
    let hits: Vec<&'a str> = known
        .iter()
        .filter(|k| k.starts_with(name))
        .copied()
        .collect();
    match hits.as_slice() {
        [] => Err(format!("unknown flag '{name}'")),
        [single] => Ok(*single),
        _ => Err(format!(
            "ambiguous flag prefix '{name}' (matches: {})",
            hits.join(", ")
        )),
    }
}

fn verb_context(tokens: &[String]) -> Result<Option<String>, String> {
    for token in tokens {
        if token == "help" || token == "-h" || token.starts_with("--") {
            continue;
        }
        return Ok(Some(canonical_verb(token)?.to_string()));
    }
    Ok(None)
}

fn take_flag(pairs: &mut Vec<(String, String)>, flag: &str, verb: &str) -> Result<String, String> {
    pairs
        .iter()
        .position(|(k, _)| k == flag)
        .map(|i| pairs.remove(i).1)
        .ok_or_else(|| format!("missing required flag '{flag}' for verb '{verb}'"))
}

fn parse_args(argv: Vec<String>) -> Result<Command, String> {
    let mut rest: Vec<String> = argv.into_iter().skip(1).collect();
    if rest.is_empty() {
        return Ok(Command::Help { verb: None });
    }
    if rest[0] == "help" {
        match rest.len() {
            1 => return Ok(Command::Help { verb: None }),
            2 => {
                let verb = canonical_verb(&rest[1])?;
                return Ok(Command::Help {
                    verb: Some(verb.to_string()),
                });
            }
            _ => return Err(format!("unexpected argument '{}'", rest[2])),
        }
    }
    // A lone `--` ends flag parsing: anything after it is a positional
    // argument, and verbs take none. Checked before the bool-flag scans so
    // hidden flags (e.g. `--dry-run` after `--`) cannot leak through.
    if let Some(pos) = rest.iter().position(|t| t == "--") {
        if pos + 1 < rest.len() {
            return Err(format!("unexpected argument '{}'", rest[pos + 1]));
        }
        rest.truncate(pos);
    }
    for token in &rest {
        if token == "-h" {
            return Ok(Command::Help {
                verb: verb_context(&rest)?,
            });
        }
        if token.starts_with("--")
            && let Ok(resolved) = resolve_flag(flag_name(token).as_str(), BOOL_FLAGS)
            && resolved == "--help"
        {
            if flag_value(token).is_some() {
                return Err("'--help' takes no value".to_string());
            }
            return Ok(Command::Help {
                verb: verb_context(&rest)?,
            });
        }
    }
    let mut dry_run = false;
    let mut kept: Vec<String> = Vec::with_capacity(rest.len());
    for token in rest {
        if token.starts_with("--")
            && let Ok(resolved) = resolve_flag(flag_name(&token).as_str(), BOOL_FLAGS)
            && resolved == "--dry-run"
        {
            if flag_value(&token).is_some() {
                return Err("'--dry-run' takes no value".to_string());
            }
            dry_run = true;
            continue;
        }
        kept.push(token);
    }
    rest = kept;
    if rest.is_empty() {
        return Ok(Command::Help { verb: None });
    }
    let verb = canonical_verb(&rest[0])?.to_string();
    let mut all_known: Vec<&str> = known_value_flags(&verb).to_vec();
    all_known.extend_from_slice(BOOL_FLAGS);
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut i = 1;
    while i < rest.len() {
        let token = rest[i].clone();
        if token == "-h" {
            return Ok(Command::Help {
                verb: Some(verb.clone()),
            });
        } else if token.starts_with("--") {
            let resolved = resolve_flag(flag_name(&token).as_str(), &all_known)
                .map_err(|e| format!("{e} for verb '{verb}'"))?
                .to_string();
            if resolved == "--dry-run" {
                dry_run = true;
            } else if resolved == "--help" {
                return Ok(Command::Help {
                    verb: Some(verb.clone()),
                });
            } else {
                let value = match flag_value(&token) {
                    Some(v) => v.to_string(),
                    // Only `--`-led tokens are reserved for flags; a lone
                    // dash-led token (e.g. `--limit -5`) flows to per-flag
                    // validation so `--limit -5` reports "invalid value"
                    // instead of "missing value" (single-dash `-h` is
                    // already claimed by the help scan above).
                    None => match rest.get(i + 1) {
                        Some(next) if !next.starts_with("--") => {
                            i += 1;
                            next.clone()
                        }
                        _ => return Err(format!("missing value for '{resolved}'")),
                    },
                };
                if value.is_empty() {
                    return Err(format!("missing value for '{resolved}'"));
                }
                if pairs.iter().any(|(k, _)| k == &resolved) {
                    return Err(format!("flag '{resolved}' given twice"));
                }
                pairs.push((resolved, value));
            }
        } else if token.starts_with('-') {
            return Err(format!("unknown flag '{token}' for verb '{verb}'"));
        } else {
            return Err(format!("unexpected argument '{token}' for verb '{verb}'"));
        }
        i += 1;
    }
    match verb.as_str() {
        "capture-lemma" => Ok(Command::CaptureLemma {
            source: take_flag(&mut pairs, "--source", &verb)?,
            dry_run,
        }),
        "capabilities" => Ok(Command::Capabilities {
            candidate: take_flag(&mut pairs, "--candidate", &verb)?,
            dry_run,
        }),
        "conformance" => Ok(Command::Conformance {
            profile: take_flag(&mut pairs, "--profile", &verb)?,
            dry_run,
        }),
        "recovery" => Ok(Command::Recovery {
            suite: take_flag(&mut pairs, "--suite", &verb)?,
            dry_run,
        }),
        "benchmark" => {
            let manifest = take_flag(&mut pairs, "--manifest", &verb)?;
            let limit = match pairs.iter().position(|(k, _)| k == "--limit") {
                Some(i) => {
                    let raw = pairs.remove(i).1;
                    Some(raw.parse::<u64>().map_err(|_| {
                        format!("invalid value '{raw}' for '--limit': expected unsigned integer")
                    })?)
                }
                None => None,
            };
            Ok(Command::Benchmark {
                manifest,
                limit,
                dry_run,
            })
        }
        "quality" => Ok(Command::Quality {
            split: take_flag(&mut pairs, "--split", &verb)?,
            dry_run,
        }),
        "evidence" => Ok(Command::Evidence {
            release: take_flag(&mut pairs, "--release", &verb)?,
            dry_run,
        }),
        _ => unreachable!("canonical verb '{verb}' has no constructor"),
    }
}

fn usage_verb(verb: &str) -> &'static str {
    match verb {
        "capture-lemma" => {
            "cargo xtask capture-lemma --source <checkout> [--dry-run]\n\
             \n\
             Capture a Lemma baseline into a sandbox (node \
             tools/capture_lemma.mjs --repo/--home/--out).\n"
        }
        "capabilities" => {
            "cargo xtask capabilities --candidate <name> [--dry-run]\n\
             \n\
             Run backend-gate suites (only fjall-lance resolves).\n"
        }
        "conformance" => {
            "cargo xtask conformance --profile <name> [--dry-run]\n\
             \n\
             Run compat + differential suites (only lemma-0.21.0 resolves).\n"
        }
        "recovery" => {
            "cargo xtask recovery --suite <name> [--dry-run]\n\
             \n\
             Run restore/kill/reopen suites (only durable resolves).\n"
        }
        "benchmark" => {
            "cargo xtask benchmark --manifest <file> [--limit N] [--dry-run]\n\
             \n\
             Run the benchmark harness (python3 tools/bench_against_ltmrs.py).\n"
        }
        "quality" => {
            "cargo xtask quality --split <name> [--dry-run]\n\
             \n\
             Run calibration corpus + quality wave (splits: heldout, dev).\n"
        }
        "evidence" => {
            "cargo xtask evidence --release <version> [--dry-run]\n\
             \n\
             Build the release evidence bundle (bash \
             tools/gen_release_evidence.sh --release --locked).\n"
        }
        _ => usage(),
    }
}

fn main() {
    match parse_args(std::env::args().collect()) {
        Ok(Command::Help { verb }) => match verb {
            Some(v) => print!("{}", usage_verb(&v)),
            None => print!("{}", usage()),
        },
        Ok(Command::Evidence { release, dry_run }) => {
            match verbs::run_evidence(&release, dry_run) {
                Ok(code) => std::process::exit(code),
                Err(err) => {
                    eprintln!("error: {err}");
                    std::process::exit(1);
                }
            }
        }
        Ok(Command::CaptureLemma { source, dry_run }) => {
            match verbs::run_capture(&source, dry_run) {
                Ok(code) => std::process::exit(code),
                Err(err) => {
                    eprintln!("error: {err}");
                    std::process::exit(1);
                }
            }
        }
        Ok(Command::Benchmark {
            manifest,
            limit,
            dry_run,
        }) => match verbs::run_benchmark(&manifest, limit, dry_run) {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
        Ok(Command::Quality { split, dry_run }) => match verbs::run_quality(&split, dry_run) {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
        Ok(Command::Capabilities { candidate, dry_run }) => {
            match verbs::run_capabilities(&candidate, dry_run) {
                Ok(code) => std::process::exit(code),
                Err(err) => {
                    eprintln!("error: {err}");
                    std::process::exit(1);
                }
            }
        }
        Ok(Command::Conformance { profile, dry_run }) => {
            match verbs::run_conformance(&profile, dry_run) {
                Ok(code) => std::process::exit(code),
                Err(err) => {
                    eprintln!("error: {err}");
                    std::process::exit(1);
                }
            }
        }
        Ok(Command::Recovery { suite, dry_run }) => match verbs::run_recovery(&suite, dry_run) {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn parses_evidence_verb() {
        let cmd = parse_args(vec![
            "xtask".into(),
            "evidence".into(),
            "--release".into(),
            "v0.1".into(),
        ])
        .unwrap();
        assert!(matches!(cmd, Command::Evidence { release, .. } if release == "v0.1"));
    }

    #[test]
    fn parses_capture_lemma_verb() {
        let cmd = parse_args(args(&["xtask", "capture-lemma", "--source", "/tmp/lemma"])).unwrap();
        assert!(matches!(cmd, Command::CaptureLemma { source, .. } if source == "/tmp/lemma"));
    }

    #[test]
    fn parses_capabilities_verb() {
        let cmd = parse_args(args(&[
            "xtask",
            "capabilities",
            "--candidate",
            "fjall-lance",
        ]))
        .unwrap();
        assert!(
            matches!(cmd, Command::Capabilities { candidate, .. } if candidate == "fjall-lance")
        );
    }

    #[test]
    fn parses_conformance_verb() {
        let cmd = parse_args(args(&["xtask", "conformance", "--profile", "lemma-0.21.0"])).unwrap();
        assert!(matches!(cmd, Command::Conformance { profile, .. } if profile == "lemma-0.21.0"));
    }

    #[test]
    fn parses_recovery_verb() {
        let cmd = parse_args(args(&["xtask", "recovery", "--suite", "durable"])).unwrap();
        assert!(matches!(cmd, Command::Recovery { suite, .. } if suite == "durable"));
    }

    #[test]
    fn parses_benchmark_verb_with_optional_limit() {
        let cmd = parse_args(args(&["xtask", "benchmark", "--manifest", "ops.json"])).unwrap();
        assert!(
            matches!(cmd, Command::Benchmark { manifest, limit: None, .. } if manifest == "ops.json")
        );
        let cmd = parse_args(args(&[
            "xtask",
            "benchmark",
            "--manifest",
            "ops.json",
            "--limit",
            "50",
        ]))
        .unwrap();
        assert!(matches!(
            cmd,
            Command::Benchmark {
                limit: Some(50),
                ..
            }
        ));
    }

    #[test]
    fn parses_quality_verb() {
        let cmd = parse_args(args(&["xtask", "quality", "--split", "heldout"])).unwrap();
        assert!(matches!(cmd, Command::Quality { split, .. } if split == "heldout"));
    }

    #[test]
    fn dry_run_flag_is_carried_alongside() {
        let cmd = parse_args(args(&[
            "xtask",
            "evidence",
            "--release",
            "v0.1",
            "--dry-run",
        ]))
        .unwrap();
        assert!(matches!(cmd, Command::Evidence { dry_run: true, .. }));
        let cmd = parse_args(args(&[
            "xtask",
            "--dry-run",
            "evidence",
            "--release",
            "v0.1",
        ]))
        .unwrap();
        assert!(matches!(cmd, Command::Evidence { dry_run: true, .. }));
    }

    #[test]
    fn unknown_verb_names_the_token() {
        let err = parse_args(args(&["xtask", "frobnicate"])).unwrap_err();
        assert!(err.contains("frobnicate"), "unexpected error: {err}");
    }

    #[test]
    fn ambiguous_verb_prefix_names_the_token() {
        let err = parse_args(args(&["xtask", "c"])).unwrap_err();
        assert!(err.contains("'c'"), "unexpected error: {err}");
    }

    #[test]
    fn unambiguous_verb_prefix_resolves() {
        let cmd = parse_args(args(&["xtask", "ev", "--release", "v0.1"])).unwrap();
        assert!(matches!(cmd, Command::Evidence { .. }));
    }

    #[test]
    fn empty_args_and_help_flags_show_help() {
        assert!(matches!(
            parse_args(args(&["xtask"])).unwrap(),
            Command::Help { verb: None }
        ));
        assert!(matches!(
            parse_args(args(&["xtask", "--help"])).unwrap(),
            Command::Help { verb: None }
        ));
        let cmd = parse_args(args(&["xtask", "evidence", "--help"])).unwrap();
        assert!(matches!(cmd, Command::Help { verb: Some(v) } if v == "evidence"));
    }

    #[test]
    fn unknown_flag_names_the_token() {
        let err = parse_args(args(&[
            "xtask",
            "evidence",
            "--bogus",
            "x",
            "--release",
            "v0.1",
        ]))
        .unwrap_err();
        assert!(err.contains("--bogus"), "unexpected error: {err}");
    }

    #[test]
    fn missing_required_flag_errors() {
        let err = parse_args(args(&["xtask", "evidence"])).unwrap_err();
        assert!(err.contains("--release"), "unexpected error: {err}");
    }

    #[test]
    fn usage_lists_all_verbs() {
        for verb in [
            "capture-lemma",
            "capabilities",
            "conformance",
            "recovery",
            "benchmark",
            "quality",
            "evidence",
        ] {
            assert!(usage().contains(verb), "usage missing {verb}");
        }
    }

    #[test]
    fn repeated_scalar_flag_errors() {
        let err = parse_args(args(&[
            "xtask",
            "evidence",
            "--release",
            "v0.1",
            "--release",
            "v0.2",
        ]))
        .unwrap_err();
        assert!(
            err.contains("given twice") && err.contains("--release"),
            "unexpected error: {err}"
        );
        // Idempotent bool flags stay repeatable.
        assert!(
            parse_args(args(&[
                "xtask",
                "evidence",
                "--release",
                "v0.1",
                "--dry-run",
                "--dry-run"
            ]))
            .is_ok()
        );
    }

    #[test]
    fn double_dash_ends_flag_parsing() {
        let err = parse_args(args(&[
            "xtask",
            "evidence",
            "--release",
            "v0.1",
            "--",
            "--dry-run",
        ]))
        .unwrap_err();
        assert!(
            err.contains("unexpected argument"),
            "unexpected error: {err}"
        );
        // Trailing -- with nothing after is ignored.
        assert!(parse_args(args(&["xtask", "evidence", "--release", "v0.1", "--"])).is_ok());
    }

    #[test]
    fn dash_leading_limit_value_reports_invalid_not_missing() {
        let err = parse_args(args(&[
            "xtask",
            "benchmark",
            "--manifest",
            "ops.json",
            "--limit",
            "-5",
        ]))
        .unwrap_err();
        assert!(err.contains("invalid value"), "unexpected error: {err}");
    }

    #[test]
    fn long_flag_as_value_is_missing_value() {
        let err = parse_args(args(&["xtask", "evidence", "--release", "--dry-run"])).unwrap_err();
        assert!(err.contains("missing value"), "unexpected error: {err}");
    }

    #[test]
    fn underscore_flags_normalize() {
        let cmd = parse_args(args(&[
            "xtask",
            "evidence",
            "--release",
            "v0.1",
            "--dry_run",
        ]))
        .unwrap();
        assert!(matches!(cmd, Command::Evidence { dry_run: true, .. }));
    }

    #[test]
    fn per_verb_help_names_verb_and_required_flag() {
        for (verb, flag) in [
            ("evidence", "--release"),
            ("capture-lemma", "--source"),
            ("capabilities", "--candidate"),
            ("conformance", "--profile"),
            ("recovery", "--suite"),
            ("benchmark", "--manifest"),
            ("quality", "--split"),
        ] {
            let text = usage_verb(verb);
            assert!(
                text.contains(verb) && text.contains(flag),
                "help for {verb} must name itself and {flag}"
            );
        }
    }

    #[test]
    fn verb_help_displays_instead_of_generic() {
        let cmd = parse_args(args(&["xtask", "evidence", "--help"])).unwrap();
        assert!(matches!(cmd, Command::Help { verb: Some(v) } if v == "evidence"));
    }
}
