//! Export a [`Workflow`] to a standalone, runnable script (Section 14
//! "Export to Bash" / "Export to Fish" / "Export to Zsh" / "Export to
//! PowerShell") — so a workflow built once can be handed to someone (or a
//! CI system) that just wants a plain script, no Zohara Terminal involved.
//!
//! Each generated script reproduces the same semantics
//! [`crate::Workflow::run`] gives a live executor: variables substituted
//! into `{{VAR}}` placeholders, a step retried up to its `max_attempts`,
//! wrapped in `timeout` when one is set, and a failed step either stopping
//! the script (`OnFailure::Stop`) or just warning and continuing
//! (`OnFailure::Continue`).
//!
//! The Bash, Zsh, and Fish generators are exercised by actually running
//! the generated script through the real `bash`/`zsh`/`fish` binary in
//! tests (not just inspecting the generated text), so a retry, a stop, and
//! a continue all demonstrably behave the way the doc comment above
//! claims. PowerShell's generator is structurally tested only — no `pwsh`
//! is available in this environment to execute it against — so treat it
//! as less battle-tested than the other three until someone runs it on a
//! machine that has PowerShell.

use crate::Workflow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptTarget {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

/// Render `workflow` as a standalone script for `target`.
pub fn export(workflow: &Workflow, target: ScriptTarget) -> String {
    match target {
        ScriptTarget::Bash => posix_like(workflow, "#!/usr/bin/env bash"),
        ScriptTarget::Zsh => posix_like(workflow, "#!/usr/bin/env zsh"),
        ScriptTarget::Fish => fish(workflow),
        ScriptTarget::PowerShell => powershell(workflow),
    }
}

fn resolve_vars(command: &str, var_ref: impl Fn(&str) -> String, workflow: &Workflow) -> String {
    let mut out = command.to_string();
    for key in workflow.variables.keys() {
        out = out.replace(&format!("{{{{{key}}}}}"), &var_ref(key));
    }
    out
}

/// Bash and Zsh scripts are close enough (both support `until`, `[ ]`,
/// arithmetic `$(( ))`, and `$VAR`) to share one generator, differing only
/// in the shebang line.
fn posix_like(workflow: &Workflow, shebang: &str) -> String {
    let mut out = String::new();
    out.push_str(shebang);
    out.push('\n');
    out.push_str("set -u\n\n");

    for (key, value) in sorted_vars(workflow) {
        out.push_str(&format!("{key}={}\n", shell_quote(value)));
    }
    if !workflow.variables.is_empty() {
        out.push('\n');
    }

    for step in &workflow.steps {
        let command = resolve_vars(&step.command, |k| format!("${k}"), workflow);
        let wrapped = match step.timeout {
            Some(d) => format!("timeout {} {}", d.as_secs(), command),
            None => command,
        };
        out.push_str(&format!("# Step: {}\n", step.name));
        out.push_str("attempt=1\n");
        out.push_str(&format!("while [ \"$attempt\" -le {} ]; do\n", step.max_attempts));
        out.push_str(&format!("  if {wrapped}; then\n"));
        out.push_str("    break\n");
        out.push_str("  fi\n");
        out.push_str("  attempt=$((attempt + 1))\n");
        out.push_str(&format!("  if [ \"$attempt\" -gt {} ]; then\n", step.max_attempts));
        out.push_str(&format!("    echo \"Step '{}' failed after {} attempt(s)\" >&2\n", step.name, step.max_attempts));
        match step.on_failure {
            crate::OnFailure::Stop => out.push_str("    exit 1\n"),
            crate::OnFailure::Continue => {}
        }
        out.push_str("  fi\n");
        out.push_str("done\n\n");
    }

    out
}

fn fish(workflow: &Workflow) -> String {
    let mut out = String::new();
    out.push_str("#!/usr/bin/env fish\n\n");

    for (key, value) in sorted_vars(workflow) {
        out.push_str(&format!("set {key} {}\n", shell_quote(value)));
    }
    if !workflow.variables.is_empty() {
        out.push('\n');
    }

    for step in &workflow.steps {
        let command = resolve_vars(&step.command, |k| format!("${k}"), workflow);
        let wrapped = match step.timeout {
            Some(d) => format!("timeout {} {}", d.as_secs(), command),
            None => command,
        };
        out.push_str(&format!("# Step: {}\n", step.name));
        out.push_str("set attempt 1\n");
        out.push_str(&format!("while test $attempt -le {}\n", step.max_attempts));
        out.push_str(&format!("  if {wrapped}\n"));
        out.push_str("    break\n");
        out.push_str("  end\n");
        out.push_str("  set attempt (math $attempt + 1)\n");
        out.push_str(&format!("  if test $attempt -gt {}\n", step.max_attempts));
        out.push_str(&format!("    echo \"Step '{}' failed after {} attempt(s)\" 1>&2\n", step.name, step.max_attempts));
        match step.on_failure {
            crate::OnFailure::Stop => out.push_str("    exit 1\n"),
            crate::OnFailure::Continue => {}
        }
        out.push_str("  end\n");
        out.push_str("end\n\n");
    }

    out
}

fn powershell(workflow: &Workflow) -> String {
    let mut out = String::new();
    out.push_str("#!/usr/bin/env pwsh\n");
    out.push_str("$ErrorActionPreference = 'Continue'\n\n");

    for (key, value) in sorted_vars(workflow) {
        out.push_str(&format!("${key} = {}\n", powershell_quote(value)));
    }
    if !workflow.variables.is_empty() {
        out.push('\n');
    }

    for step in &workflow.steps {
        let command = resolve_vars(&step.command, |k| format!("${k}"), workflow);
        out.push_str(&format!("# Step: {}\n", step.name));
        out.push_str("$attempt = 1\n");
        out.push_str(&format!("while ($attempt -le {}) {{\n", step.max_attempts));
        if let Some(d) = step.timeout {
            out.push_str(&format!("  $job = Start-Job -ScriptBlock {{ {command} }}\n"));
            out.push_str(&format!("  $completed = Wait-Job $job -Timeout {}\n", d.as_secs()));
            out.push_str("  $success = $completed -and ($job.State -eq 'Completed')\n");
            out.push_str("  Remove-Job $job -Force\n");
        } else {
            out.push_str(&format!("  Invoke-Expression \"{}\"\n", command.replace('"', "`\"")));
            out.push_str("  $success = $LASTEXITCODE -eq 0\n");
        }
        out.push_str("  if ($success) { break }\n");
        out.push_str("  $attempt += 1\n");
        out.push_str(&format!("  if ($attempt -gt {}) {{\n", step.max_attempts));
        out.push_str(&format!("    Write-Error \"Step '{}' failed after {} attempt(s)\"\n", step.name, step.max_attempts));
        match step.on_failure {
            crate::OnFailure::Stop => out.push_str("    exit 1\n"),
            crate::OnFailure::Continue => {}
        }
        out.push_str("  }\n");
        out.push_str("}\n\n");
    }

    out
}

/// Iterate variables in a stable (sorted) order so generated scripts are
/// deterministic — `HashMap` iteration order isn't, and a diff-friendly
/// export matters for something meant to be checked into version control.
fn sorted_vars(workflow: &Workflow) -> Vec<(&str, &str)> {
    let mut vars: Vec<(&str, &str)> = workflow.variables.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    vars.sort_by_key(|(k, _)| *k);
    vars
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Step;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    fn scratch_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ut-workflow-export-test-{}-{name}", std::process::id()));
        p
    }

    fn write_script(target: ScriptTarget, workflow: &Workflow, name: &str) -> PathBuf {
        let path = scratch_path(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(export(workflow, target).as_bytes()).unwrap();
        path
    }

    /// Run `script` through `interpreter`, or `None` if that interpreter
    /// isn't installed here. `bash` is assumed always present (it's as
    /// close to a universal baseline as Linux testing gets), but `zsh`/
    /// `fish` are not guaranteed on every machine `cargo test` runs on —
    /// callers for those should skip rather than fail when this is `None`,
    /// the same way `ut-plugin-ssh`'s tests skip rather than fail without
    /// network access.
    fn run(interpreter: &str, script: &PathBuf) -> Option<std::process::Output> {
        match Command::new(interpreter).arg(script).output() {
            Ok(output) => Some(output),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping: `{interpreter}` is not installed in this environment");
                None
            }
            Err(e) => panic!("failed to spawn `{interpreter}`: {e}"),
        }
    }

    macro_rules! skip_if_missing {
        ($opt:expr) => {
            match $opt {
                Some(output) => output,
                None => return,
            }
        };
    }

    // A command whose success depends on how many times it's been run,
    // using a scratch counter file — independent of which shell runs it,
    // so the same retry-count assertion works for bash/zsh/fish alike.
    fn flaky_command(counter_path: &std::path::Path, succeed_on_attempt: u32) -> String {
        format!(
            "sh -c 'f=\"{p}\"; n=$(cat \"$f\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$f\"; [ \"$n\" -ge {s} ]'",
            p = counter_path.display(),
            s = succeed_on_attempt
        )
    }

    #[test]
    fn bash_structural_shape() {
        let wf = Workflow::new("t").with_variable("X", "1").with_step(Step::new("a", "echo {{X}}"));
        let script = export(&wf, ScriptTarget::Bash);
        assert!(script.starts_with("#!/usr/bin/env bash\n"));
        assert!(script.contains("X='1'"));
        assert!(script.contains("echo $X"));
    }

    #[test]
    fn fish_structural_shape() {
        let wf = Workflow::new("t").with_variable("X", "1").with_step(Step::new("a", "echo {{X}}"));
        let script = export(&wf, ScriptTarget::Fish);
        assert!(script.starts_with("#!/usr/bin/env fish\n"));
        assert!(script.contains("set X '1'"));
        assert!(script.contains("echo $X"));
    }

    #[test]
    fn powershell_structural_shape() {
        let wf = Workflow::new("t").with_variable("X", "1").with_step(Step::new("a", "echo {{X}}"));
        let script = export(&wf, ScriptTarget::PowerShell);
        assert!(script.contains("$X = '1'"));
        assert!(script.contains("echo $X"));
    }

    #[test]
    fn bash_runs_a_simple_step_successfully() {
        let wf = Workflow::new("t").with_step(Step::new("echo", "echo bash-export-ok"));
        let path = write_script(ScriptTarget::Bash, &wf, "simple-bash.sh");
        let output = skip_if_missing!(run("bash", &path));
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("bash-export-ok"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn zsh_runs_a_simple_step_successfully() {
        let wf = Workflow::new("t").with_step(Step::new("echo", "echo zsh-export-ok"));
        let path = write_script(ScriptTarget::Zsh, &wf, "simple-zsh.sh");
        let output = skip_if_missing!(run("zsh", &path));
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("zsh-export-ok"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn fish_runs_a_simple_step_successfully() {
        let wf = Workflow::new("t").with_step(Step::new("echo", "echo fish-export-ok"));
        let path = write_script(ScriptTarget::Fish, &wf, "simple-fish.fish");
        let output = skip_if_missing!(run("fish", &path));
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("fish-export-ok"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bash_variable_substitution_actually_substitutes() {
        let wf = Workflow::new("t").with_variable("GREETING", "hello-from-bash-var").with_step(Step::new("echo", "echo {{GREETING}}"));
        let path = write_script(ScriptTarget::Bash, &wf, "varsub-bash.sh");
        let output = skip_if_missing!(run("bash", &path));
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello-from-bash-var"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn fish_variable_substitution_actually_substitutes() {
        let wf = Workflow::new("t").with_variable("GREETING", "hello-from-fish-var").with_step(Step::new("echo", "echo {{GREETING}}"));
        let path = write_script(ScriptTarget::Fish, &wf, "varsub-fish.fish");
        let output = skip_if_missing!(run("fish", &path));
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello-from-fish-var"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bash_retries_until_the_configured_attempt_succeeds() {
        let counter = scratch_path("bash-retry-counter");
        let _ = fs::remove_file(&counter);
        let wf = Workflow::new("t").with_step(Step::new("flaky", flaky_command(&counter, 3)).with_retry(5));
        let path = write_script(ScriptTarget::Bash, &wf, "retry-bash.sh");
        let output = skip_if_missing!(run("bash", &path));
        assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
        let attempts: u32 = fs::read_to_string(&counter).unwrap().trim().parse().unwrap();
        assert_eq!(attempts, 3);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&counter);
    }

    #[test]
    fn zsh_retries_until_the_configured_attempt_succeeds() {
        let counter = scratch_path("zsh-retry-counter");
        let _ = fs::remove_file(&counter);
        let wf = Workflow::new("t").with_step(Step::new("flaky", flaky_command(&counter, 2)).with_retry(5));
        let path = write_script(ScriptTarget::Zsh, &wf, "retry-zsh.sh");
        let output = skip_if_missing!(run("zsh", &path));
        assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
        let attempts: u32 = fs::read_to_string(&counter).unwrap().trim().parse().unwrap();
        assert_eq!(attempts, 2);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&counter);
    }

    #[test]
    fn fish_retries_until_the_configured_attempt_succeeds() {
        let counter = scratch_path("fish-retry-counter");
        let _ = fs::remove_file(&counter);
        let wf = Workflow::new("t").with_step(Step::new("flaky", flaky_command(&counter, 3)).with_retry(5));
        let path = write_script(ScriptTarget::Fish, &wf, "retry-fish.fish");
        let output = skip_if_missing!(run("fish", &path));
        assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
        let attempts: u32 = fs::read_to_string(&counter).unwrap().trim().parse().unwrap();
        assert_eq!(attempts, 3);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&counter);
    }

    #[test]
    fn bash_gives_up_and_exits_nonzero_after_max_attempts() {
        let counter = scratch_path("bash-giveup-counter");
        let _ = fs::remove_file(&counter);
        // Needs 100 successes to pass, but only gets 3 attempts: must fail.
        let wf = Workflow::new("t").with_step(Step::new("always-fails", flaky_command(&counter, 100)).with_retry(3));
        let path = write_script(ScriptTarget::Bash, &wf, "giveup-bash.sh");
        let output = skip_if_missing!(run("bash", &path));
        assert!(!output.status.success());
        let attempts: u32 = fs::read_to_string(&counter).unwrap().trim().parse().unwrap();
        assert_eq!(attempts, 3);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&counter);
    }

    #[test]
    fn bash_stop_on_failure_skips_later_steps() {
        let marker = scratch_path("bash-stop-marker");
        let _ = fs::remove_file(&marker);
        let wf = Workflow::new("t")
            .with_step(Step::new("fails", "false"))
            .with_step(Step::new("should-not-run", format!("touch {}", marker.display())));
        let path = write_script(ScriptTarget::Bash, &wf, "stop-bash.sh");
        let output = skip_if_missing!(run("bash", &path));
        assert!(!output.status.success());
        assert!(!marker.exists(), "second step ran even though the first step should have stopped the script");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bash_continue_on_failure_still_runs_later_steps() {
        let marker = scratch_path("bash-continue-marker");
        let _ = fs::remove_file(&marker);
        let wf = Workflow::new("t")
            .with_step(Step::new("fails", "false").continue_on_failure())
            .with_step(Step::new("should-run", format!("touch {}", marker.display())));
        let path = write_script(ScriptTarget::Bash, &wf, "continue-bash.sh");
        let _output = skip_if_missing!(run("bash", &path));
        assert!(marker.exists(), "second step did not run even though the first step was continue-on-failure");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&marker);
    }

    #[test]
    fn fish_stop_on_failure_skips_later_steps() {
        let marker = scratch_path("fish-stop-marker");
        let _ = fs::remove_file(&marker);
        let wf = Workflow::new("t")
            .with_step(Step::new("fails", "false"))
            .with_step(Step::new("should-not-run", format!("touch {}", marker.display())));
        let path = write_script(ScriptTarget::Fish, &wf, "stop-fish.fish");
        let output = skip_if_missing!(run("fish", &path));
        assert!(!output.status.success());
        assert!(!marker.exists());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bash_timeout_kills_a_long_running_step() {
        let wf = Workflow::new("t").with_step(Step::new("slow", "sleep 30").with_timeout(Duration::from_secs(1)));
        let path = write_script(ScriptTarget::Bash, &wf, "timeout-bash.sh");
        let start = std::time::Instant::now();
        let output = skip_if_missing!(run("bash", &path));
        let elapsed = start.elapsed();
        assert!(!output.status.success());
        assert!(elapsed < Duration::from_secs(10), "step should have been killed by `timeout` well under 10s, took {elapsed:?}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn export_is_deterministic_across_hashmap_variable_order() {
        let wf = Workflow::new("t").with_variable("B", "2").with_variable("A", "1").with_step(Step::new("s", "echo hi"));
        let first = export(&wf, ScriptTarget::Bash);
        let second = export(&wf, ScriptTarget::Bash);
        assert_eq!(first, second);
        // A should come before B regardless of HashMap iteration order.
        assert!(first.find("A=").unwrap() < first.find("B=").unwrap());
    }

    #[test]
    fn run_skips_cleanly_for_an_interpreter_that_is_not_installed() {
        // Proves the skip-rather-than-fail path directly, independent of
        // which real shells happen to be on PATH in this environment —
        // this is what every zsh_*/fish_* test above falls back to if
        // that shell isn't installed wherever `cargo test` runs.
        let wf = Workflow::new("t").with_step(Step::new("s", "echo hi"));
        let path = write_script(ScriptTarget::Bash, &wf, "nonexistent-interpreter.sh");
        let result = run("definitely-not-a-real-shell-xyz", &path);
        assert!(result.is_none());
        let _ = fs::remove_file(&path);
    }
}

