//! Engine plumbing: parsing/evaluating Nushell source, exported-const reads, build
//! failure shaping, and the streamed build log.

use anyhow::{Context, Result, anyhow};
use nu_protocol::{
    PipelineData, ShellError, Value,
    debugger::WithoutDebug,
    engine::{Stack, StateWorkingSet},
    process::check_exit_status_future,
};
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use crate::util::progress;

use super::*;

pub(crate) fn exported_const(path: &Path, name: &str) -> Result<Value> {
    let source = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut engine_state = nu_cmd_lang::create_default_context();
    engine_state.add_env_var("PWD".to_string(), Value::test_string("."));
    let mut working_set = StateWorkingSet::new(&engine_state);
    nu_parser::parse(&mut working_set, path.to_str(), &source, false);

    if let Some(err) = working_set.parse_errors.first() {
        return Err(anyhow!(
            "could not parse {}: {err} ({err:?})",
            path.display()
        ));
    }

    let var_id = working_set
        .find_variable(name.as_bytes())
        .ok_or_else(|| anyhow!("{} does not export const `{name}`", path.display()))?;
    let value = working_set.get_constant(var_id).map_err(|err| {
        anyhow!(
            "could not read exported const `{name}` from {}: {err}",
            path.display()
        )
    })?;

    Ok(value.clone())
}

pub(crate) fn eval_nu_source(
    source: &str,
    source_name: Option<&str>,
    cwd: &Path,
    path: Option<&str>,
    extra_env: &[(String, String)],
    log_path: Option<PathBuf>,
) -> Result<Option<Value>> {
    let _working_dir = WorkingDirectoryGuard::enter(cwd)?;
    let mut engine_state =
        crate::nu::commands::add_rune_command_context(nu_cmd_lang::create_default_context());
    engine_state.add_env_var(
        "PWD".to_string(),
        Value::string(cwd.display().to_string(), nu_protocol::Span::unknown()),
    );
    let mut streamer = BuildLogStreamer::start(log_path)?;
    let mut stack = streamer.configure_stack(Stack::new())?;
    stack.add_env_var(
        "PWD".to_string(),
        Value::string(cwd.display().to_string(), nu_protocol::Span::unknown()),
    );
    if let Some(path) = path {
        let value = path_value_from_string(path);
        engine_state.add_env_var("PATH".to_string(), value.clone());
        stack.add_env_var("PATH".to_string(), value);
    }
    for (key, value) in extra_env {
        let v = Value::string(value, nu_protocol::Span::unknown());
        engine_state.add_env_var(key.clone(), v.clone());
        stack.add_env_var(key.clone(), v);
    }

    let (block, delta) = {
        let mut working_set = StateWorkingSet::new(&engine_state);
        let block = nu_parser::parse(&mut working_set, source_name, source.as_bytes(), false);

        if let Some(err) = working_set.parse_errors.first() {
            return Err(anyhow!(
                "could not parse embedded Nushell build runner: {err}"
            ));
        }
        if let Some(err) = working_set.compile_errors.first() {
            return Err(anyhow!(
                "could not compile embedded Nushell build runner: {err}"
            ));
        }

        (block, working_set.render())
    };

    engine_state
        .merge_delta(delta)
        .context("merge embedded Nushell build runner")?;
    let eval_result = nu_engine::eval_block::<WithoutDebug>(
        &engine_state,
        &mut stack,
        &block,
        PipelineData::empty(),
    );
    // Drop the stack to close our copies of the pipe writers; the child still holds its ends until
    // it exits, which the exit-status check below waits for.
    drop(stack);

    // Resolve the outcome before joining the log readers. Returning `eval_block`'s result is not
    // enough: a build whose *final* command is a failing external (e.g. a `sh -c "./configure ..."`
    // that exits non-zero) leaves the failure in the pipeline's exit status, not as an error value.
    // Draining the body and checking each element's exit status — exactly what Nushell's own
    // `eval_source` does for pipefail — surfaces it, so a failed build aborts instead of packing a
    // broken archive. `complete`-captured commands mark themselves handled and do not trip this.
    let (outcome, last_value) = match eval_result {
        Err(error) => (Err(error), None),
        Ok(output) => match output.body {
            PipelineData::Value(Value::Error { error, .. }, ..) => (Err(*error), None),
            PipelineData::Value(value, ..) => {
                let outcome = check_exit_status_future(output.exit);
                (outcome, Some(value))
            }
            body => match body.drain() {
                Ok(()) => (check_exit_status_future(output.exit), None),
                Err(error) => (Err(error), None),
            },
        },
    };
    let (tail, log_path) = streamer.finish();

    if outcome.is_ok()
        && let Some(path) = &log_path
    {
        let _ = fs::remove_file(path);
    }

    outcome.map_err(|error| build_failure(&error, &tail, log_path.as_deref()))?;
    Ok(last_value)
}

/// Turns a build-time [`ShellError`] into a reportable error: a one-line Nushell diagnostic
/// (exit code / signal where we have it, otherwise the error's own message) followed by the last
/// few lines of the build's own stdout/stderr. The full log is written to a file and its path is
/// included so the user can inspect the complete output.
pub(crate) fn build_failure(
    error: &ShellError,
    tail: &[String],
    log_path: Option<&Path>,
) -> anyhow::Error {
    let mut message = format!(
        "embedded Nushell build failed: {}",
        describe_shell_error(error)
    );
    if !tail.is_empty() {
        let start = tail.len().saturating_sub(10);
        message.push_str("\nlast build output:");
        for line in &tail[start..] {
            message.push_str("\n    ");
            message.push_str(line);
        }
    }
    if let Some(path) = log_path {
        message.push_str(&format!("\nfull build log: {}", path.display()));
    }
    anyhow!(message)
}

/// The terse `Display` of [`ShellError`] collapses external-command failures to "External command
/// had a non-zero exit code". Pull the diagnostic detail (exit code / signal) out of the variants
/// that carry it so the surfaced error names what actually happened.
pub(crate) fn describe_shell_error(error: &ShellError) -> String {
    match error {
        ShellError::NonZeroExitCode { exit_code, .. } => {
            format!("external command exited with code {exit_code}")
        }
        ShellError::TerminatedBySignal {
            signal_name,
            signal,
            ..
        } => format!("external command was terminated by {signal_name} ({signal})"),
        other => other.to_string(),
    }
}

/// How many trailing build-output lines to retain for error reports. Enough to capture a typical
/// `configure`/compiler failure without dumping the whole log into the error.
pub(crate) const BUILD_TAIL_LINES: usize = 100;

/// Shared ring buffer of the most recent build-output lines, filled by the reader threads
/// regardless of verbosity so a failed build can report what went wrong.
type BuildTail = Arc<Mutex<VecDeque<String>>>;

pub(crate) struct BuildLogStreamer {
    stdout: Option<thread::JoinHandle<()>>,
    stderr: Option<thread::JoinHandle<()>>,
    stdout_writer: Option<std::fs::File>,
    stderr_writer: Option<std::fs::File>,
    tail: BuildTail,
    log_path: Option<PathBuf>,
}

impl BuildLogStreamer {
    fn start(log_path: Option<PathBuf>) -> Result<Self> {
        let log_file = log_path.as_ref().and_then(|path| {
            fs::create_dir_all(path.parent()?).ok()?;
            fs::File::create(path).ok().map(|f| Arc::new(Mutex::new(f)))
        });

        let (stdout_reader, stdout_writer) = os_pipe::pipe().context("create build stdout pipe")?;
        let (stderr_reader, stderr_writer) = os_pipe::pipe().context("create build stderr pipe")?;
        let tail: BuildTail = Arc::new(Mutex::new(VecDeque::with_capacity(BUILD_TAIL_LINES)));
        Ok(Self {
            stdout: Some(spawn_build_log_reader(
                stdout_reader,
                Arc::clone(&tail),
                log_file.clone(),
            )),
            stderr: Some(spawn_build_log_reader(
                stderr_reader,
                Arc::clone(&tail),
                log_file.clone(),
            )),
            stdout_writer: Some(pipe_writer_file(stdout_writer)),
            stderr_writer: Some(pipe_writer_file(stderr_writer)),
            tail,
            log_path,
        })
    }

    fn configure_stack(&mut self, stack: Stack) -> Result<Stack> {
        let stdout = self
            .stdout_writer
            .take()
            .context("build stdout writer was already consumed")?;
        let stderr = self
            .stderr_writer
            .take()
            .context("build stderr writer was already consumed")?;
        Ok(stack.reset_pipes().stdout_file(stdout).stderr_file(stderr))
    }

    /// Joins the reader threads (so the tail holds all drained output) and returns the retained
    /// trailing lines for error reporting together with the log file path.
    fn finish(mut self) -> (Vec<String>, Option<PathBuf>) {
        self.join_handles();
        let tail = match self.tail.lock() {
            Ok(tail) => tail.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        };
        (tail, self.log_path.clone())
    }

    fn join_handles(&mut self) {
        if let Some(handle) = self.stdout.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BuildLogStreamer {
    fn drop(&mut self) {
        self.join_handles();
    }
}

pub(crate) fn spawn_build_log_reader(
    reader: os_pipe::PipeReader,
    tail: BuildTail,
    log_file: Option<Arc<Mutex<fs::File>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Ok(mut tail) = tail.lock() {
                if tail.len() == BUILD_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line.clone());
            }
            progress::build_log_line(&line);
            if let Some(file) = &log_file
                && let Ok(mut f) = file.lock()
            {
                let _ = writeln!(f, "{}", line);
                let _ = f.flush();
            }
        }
    })
}

pub(crate) fn pipe_writer_file(writer: os_pipe::PipeWriter) -> std::fs::File {
    use std::os::fd::OwnedFd;

    OwnedFd::from(writer).into()
}

pub(crate) struct WorkingDirectoryGuard {
    previous: PathBuf,
}

impl WorkingDirectoryGuard {
    fn enter(cwd: &Path) -> Result<Self> {
        let previous = std::env::current_dir().context("read current working directory")?;
        std::env::set_current_dir(cwd)
            .with_context(|| format!("enter build working directory {}", cwd.display()))?;
        Ok(Self { previous })
    }
}

impl Drop for WorkingDirectoryGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_export_const_parse() -> Result<()> {
        let mut engine_state = nu_cmd_lang::create_default_context();
        engine_state.add_env_var("PWD".to_string(), nu_protocol::Value::test_string("."));
        let mut working_set = StateWorkingSet::new(&engine_state);
        let block = nu_parser::parse(
            &mut working_set,
            None,
            b"export const package = { name: hello, version: \"0.1.0\", bins: { hello: \"bin/hello\" } }\n",
            false,
        );

        assert!(
            working_set.parse_errors.is_empty(),
            "{:?}",
            working_set.parse_errors
        );
        assert_eq!(block.pipelines.len(), 1);
        let var_id = working_set
            .find_variable(b"package")
            .ok_or_else(|| anyhow!("package variable not found"))?;
        let value = working_set
            .get_constant(var_id)
            .map_err(|err| anyhow!("package const: {err}"))?;
        eprintln!("{value:#?}");
        Ok(())
    }
}
