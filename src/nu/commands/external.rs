//! External command execution for the embedded rune engine.
//!
//! Adapted from nu-command's `run-external` and `complete` (MIT, © Nushell contributors),
//! stripped to what rune builds need: no Windows branches, no hooks, no job control, no
//! interactive foregrounding. Redirection honors the stack's `OutDest`s, which is how the
//! build-log streamer captures everything externals print.

// ShellError is nushell's error type and large by its design; boxing it everywhere would
// diverge from the upstream Command trait shapes this module mirrors.
#![allow(clippy::result_large_err)]

use nu_engine::{command_prelude::*, env_to_strings};
use nu_path::{dots::expand_ndots_safe, expand_tilde};
use nu_protocol::{
    ByteStream, NuGlob, OutDest, Signals,
    process::ChildProcess,
    shell_error::{generic::GenericError, io::IoError},
};
use nu_system::ForegroundChild;
use std::{
    borrow::Cow,
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    thread,
};

#[derive(Clone)]
pub struct External;

impl Command for External {
    fn name(&self) -> &str {
        "run-external"
    }

    fn description(&self) -> &str {
        "Runs an external command."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Any, Type::Any)])
            .rest(
                "command",
                SyntaxShape::OneOf(vec![SyntaxShape::GlobPattern, SyntaxShape::Any]),
                "External command to run, with arguments.",
            )
            .category(Category::System)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let cwd = engine_state.cwd(Some(stack))?;
        let rest = call.rest::<Value>(engine_state, stack, 0)?;
        let Some((name, call_args)) = rest.split_first().map(|(x, y)| (x, y.to_vec())) else {
            return Err(ShellError::MissingParameter {
                param_name: "no command given".into(),
                span: call.head,
            });
        };

        let name_str: Cow<str> = match &name {
            Value::Glob { val, .. } => Cow::Borrowed(val),
            Value::String { val, .. } => Cow::Borrowed(val),
            _ => Cow::Owned(name.clone().coerce_into_string()?),
        };
        let expanded_name = match &name {
            Value::Glob { no_expand, .. } if !*no_expand => {
                expand_ndots_safe(expand_tilde(&*name_str))
            }
            _ => Path::new(&*name_str).to_owned(),
        };

        let paths = nu_engine::env::path_str(engine_state, stack, call.head).unwrap_or_default();
        let Some(executable) = which(&expanded_name, &paths, cwd.as_std_path()) else {
            return Err(ShellError::Generic(GenericError::new(
                format!("command not found: {name_str}"),
                "not found on the managed build PATH (declare missing tools in `deps.build`)",
                call.head,
            )));
        };

        let mut command = std::process::Command::new(&executable);
        command.current_dir(&cwd);
        command.env_clear();
        command.envs(env_to_strings(engine_state, stack)?);
        command.args(
            eval_external_arguments(engine_state, stack, call_args)?
                .into_iter()
                .map(|s| s.item),
        );

        // Configure stdout/stderr from the stack's redirection targets. When both are
        // `Pipe`, merge them into one stream like upstream does.
        let stdout = stack.stdout();
        let stderr = stack.stderr();
        let merged_stream = if matches!(stdout, OutDest::Pipe) && matches!(stderr, OutDest::Pipe) {
            let (reader, writer) =
                os_pipe::pipe().map_err(|err| IoError::new(err, call.head, None))?;
            command.stdout(
                writer
                    .try_clone()
                    .map_err(|err| IoError::new(err, call.head, None))?,
            );
            command.stderr(writer);
            Some(reader)
        } else {
            command
                .stdout(Stdio::try_from(stdout).map_err(|err| IoError::new(err, call.head, None))?);
            command
                .stderr(Stdio::try_from(stderr).map_err(|err| IoError::new(err, call.head, None))?);
            None
        };

        // Connect stdin: bytestreams attach directly when possible, other values are piped
        // in from a writer thread, and empty input inherits.
        let data_to_copy_into_stdin = match input {
            PipelineData::ByteStream(stream, metadata) => match stream.into_stdio() {
                Ok(stdin) => {
                    command.stdin(stdin);
                    None
                }
                Err(stream) => {
                    command.stdin(Stdio::piped());
                    Some(PipelineData::byte_stream(stream, metadata))
                }
            },
            PipelineData::Empty => {
                command.stdin(Stdio::inherit());
                None
            }
            value => {
                command.stdin(Stdio::piped());
                Some(value)
            }
        };

        let mut child = ForegroundChild::spawn(
            command,
            engine_state.is_interactive,
            false,
            &engine_state.pipeline_externals_state,
        )
        .map_err(|err| {
            let context = format!("could not spawn external command: {err}");
            IoError::new_internal(err, context)
        })?;

        if let Some(data) = data_to_copy_into_stdin {
            let stdin = child.as_mut().stdin.take().expect("stdin is piped");
            thread::Builder::new()
                .name("external stdin worker".into())
                .spawn(move || {
                    let _ = write_pipeline_data(data, stdin);
                })
                .map_err(|err| {
                    IoError::new_with_additional_context(
                        err,
                        call.head,
                        None,
                        "could not spawn external stdin worker",
                    )
                })?;
        }

        let child = ChildProcess::new(
            child,
            merged_stream,
            matches!(stderr, OutDest::Pipe),
            call.head,
            None,
        )?;
        Ok(PipelineData::byte_stream(
            ByteStream::child(child, call.head),
            None,
        ))
    }
}

/// Evaluates external arguments: bare-word globs expand (tilde, ndots, and glob matching);
/// everything else is coerced to a string verbatim.
fn eval_external_arguments(
    engine_state: &EngineState,
    stack: &mut Stack,
    call_args: Vec<Value>,
) -> Result<Vec<Spanned<OsString>>, ShellError> {
    let cwd = engine_state.cwd(Some(stack))?;
    let mut args: Vec<Spanned<OsString>> = Vec::with_capacity(call_args.len());
    for arg in call_args {
        let span = arg.span();
        match arg {
            Value::Glob { val, no_expand, .. } if !no_expand => args.extend(
                expand_glob(
                    &val,
                    cwd.as_std_path(),
                    span,
                    engine_state.signals().clone(),
                )?
                .into_iter()
                .map(|s| s.into_spanned(span)),
            ),
            Value::Glob { val, .. } => args.push(OsString::from(val).into_spanned(span)),
            Value::List { .. } => {
                return Err(ShellError::Generic(GenericError::new(
                    "cannot pass a list as an external argument",
                    "spread the list or pass elements individually",
                    span,
                )));
            }
            other => {
                args.push(OsString::from(other.coerce_into_string()?).into_spanned(span));
            }
        }
    }
    Ok(args)
}

/// Glob expansion matching upstream's behavior: a non-glob argument only gets tilde/ndots
/// expansion, an unmatched glob passes through verbatim.
fn expand_glob(
    arg: &str,
    cwd: &Path,
    span: Span,
    signals: Signals,
) -> Result<Vec<OsString>, ShellError> {
    if !nu_glob::is_glob(arg) {
        let path = expand_ndots_safe(expand_tilde(arg));
        return Ok(vec![path.into()]);
    }
    let glob = NuGlob::Expand(arg.to_owned()).into_spanned(span);
    if let Ok((_prefix, matches)) = nu_engine::glob_from(&glob, cwd, span, None, signals.clone()) {
        let mut result: Vec<OsString> = Vec::new();
        for m in matches {
            signals.check(&span)?;
            match m {
                Ok(path) => result.push(path.into_os_string()),
                Err(_) => result.push(arg.into()),
            }
        }
        if result.is_empty() {
            result.push(arg.into());
        }
        Ok(result)
    } else {
        Ok(vec![arg.into()])
    }
}

fn write_pipeline_data(data: PipelineData, mut writer: impl Write) -> Result<(), ShellError> {
    match data {
        PipelineData::ByteStream(stream, ..) => stream.write_to(writer)?,
        PipelineData::Value(Value::Binary { val, .. }, ..) => writer
            .write_all(&val)
            .map_err(|err| IoError::new_internal(err, "could not write pipeline data"))?,
        data => {
            // The rune engine has no `table` renderer; coerce values to plain strings.
            for value in data {
                let text = value.coerce_into_string()?;
                writer
                    .write_all(text.as_bytes())
                    .map_err(|err| IoError::new_internal(err, "could not write pipeline data"))?;
            }
        }
    }
    Ok(())
}

/// Resolves `name` against the build PATH: absolute/relative paths with separators are used
/// as-is (relative to `cwd`), bare names are searched in PATH order requiring the executable
/// bit. No `which` crate needed.
pub(super) fn which(name: &Path, paths: &str, cwd: &Path) -> Option<PathBuf> {
    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    if name.components().count() > 1 || name.is_absolute() {
        let candidate = if name.is_absolute() {
            name.to_owned()
        } else {
            cwd.join(name)
        };
        return is_executable(&candidate).then_some(candidate);
    }
    for dir in paths.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[derive(Clone)]
pub struct Complete;

impl Command for Complete {
    fn name(&self) -> &str {
        "complete"
    }

    fn description(&self) -> &str {
        "Captures an external command's stdout, stderr, and exit code into a record."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Any, Type::record())])
            .category(Category::System)
    }

    fn run(
        &self,
        _engine_state: &EngineState,
        _stack: &mut Stack,
        call: &Call,
        input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let head = call.head;
        match input {
            PipelineData::ByteStream(stream, ..) => {
                let Ok(mut child) = stream.into_child() else {
                    return Err(ShellError::Generic(GenericError::new(
                        "complete only works with external commands",
                        "not an external command",
                        head,
                    )));
                };
                // `complete` reports the status via its `exit_code` field; mark the child
                // handled so pipefail does not also raise on a non-zero exit.
                child.ignore_error(true);
                let output = child.wait_with_output()?;
                let exit_code = output.exit_status.code();
                let mut record = Record::new();
                if let Some(stdout) = output.stdout {
                    record.push(
                        "stdout",
                        match String::from_utf8(stdout) {
                            Ok(str) => Value::string(str, head),
                            Err(err) => Value::binary(err.into_bytes(), head),
                        },
                    );
                }
                if let Some(stderr) = output.stderr {
                    record.push(
                        "stderr",
                        match String::from_utf8(stderr) {
                            Ok(str) => Value::string(str, head),
                            Err(err) => Value::binary(err.into_bytes(), head),
                        },
                    );
                }
                record.push("exit_code", Value::int(exit_code.into(), head));
                Ok(Value::record(record, head).into_pipeline_data())
            }
            _ => Err(ShellError::Generic(GenericError::new(
                "complete only works with external commands",
                "not an external command",
                head,
            ))),
        }
    }
}
