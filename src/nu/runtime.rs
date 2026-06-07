//! Evaluating `.rn` definitions and running build steps in the embedded Nushell engine.
//!
//! The [`RuneRuntime`] trait exposes reading package/tome metadata and executing a rune's `build`
//! function against a prepared context; [`EmbeddedNuRuntime`] is the in-process implementation.
//! Runes are evaluated, not shelled out to — the engine is embedded (AGENTS.md §1a).

use anyhow::{Context, Result, anyhow};
use nu_protocol::{
    PipelineData, Record, ShellError, Span, Value,
    debugger::WithoutDebug,
    engine::{Stack, StateWorkingSet},
    process::check_exit_status_future,
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use crate::{
    fetch::FetchedSource,
    model::{BuildManifest, PackageMetadata, TomeManifest},
    nu::nuon_io,
    paths, progress,
    toolchain::HostTool,
};

pub trait RuneRuntime {
    fn package_metadata(&self, rune: &Path) -> Result<PackageMetadata>;
    fn tome_manifest(&self, tome: &Path) -> Result<TomeManifest>;
    fn build(
        &self,
        rune: &Path,
        dirs: &BuildDirs,
        sources: &BTreeMap<String, FetchedSource>,
        build_flags: &BTreeMap<String, String>,
        env: &BuildEnv,
    ) -> Result<Option<BuildManifest>>;
}

#[derive(Debug, Default)]
pub struct EmbeddedNuRuntime;

#[derive(Debug)]
pub struct BuildEnv {
    pub path_dirs: Vec<PathBuf>,
    pub host_tools: Vec<HostTool>,
    pub inherit_host_path: bool,
    /// Additional environment variables to set in the build sandbox.
    pub extra_env: Vec<(String, String)>,
    /// Target triple the build is being performed for.
    pub target: String,
}

#[derive(Debug)]
pub struct BuildDirs {
    pub package_dir: PathBuf,
    pub final_prefix: PathBuf,
    pub work_dir: PathBuf,
    pub log_file: Option<PathBuf>,
}

impl BuildEnv {
    /// Stage-0 authoring/bootstrap builds inherit the host PATH but still include installed
    /// build dependencies so later packages in a tome can find seeds built earlier.
    pub fn bootstrap(path_dirs: Vec<PathBuf>, extra_env: Vec<(String, String)>) -> Self {
        Self {
            path_dirs,
            host_tools: Vec::new(),
            inherit_host_path: true,
            extra_env,
            target: paths::target_triple(),
        }
    }

    pub fn managed(
        path_dirs: Vec<PathBuf>,
        host_tools: Vec<HostTool>,
        extra_env: Vec<(String, String)>,
    ) -> Self {
        Self {
            path_dirs,
            host_tools,
            inherit_host_path: false,
            extra_env,
            target: paths::target_triple(),
        }
    }
}

impl Default for BuildEnv {
    fn default() -> Self {
        Self::bootstrap(Vec::new(), Vec::new())
    }
}

impl RuneRuntime for EmbeddedNuRuntime {
    fn package_metadata(&self, rune: &Path) -> Result<PackageMetadata> {
        PackageMetadata::from_value(exported_const(rune, "package")?, false)
            .with_context(|| format!("parse package metadata from {}", rune.display()))
    }

    fn tome_manifest(&self, tome: &Path) -> Result<TomeManifest> {
        TomeManifest::from_value(exported_const(tome, "tome")?)
            .with_context(|| format!("parse tome manifest from {}", tome.display()))
    }

    fn build(
        &self,
        rune: &Path,
        dirs: &BuildDirs,
        sources: &BTreeMap<String, FetchedSource>,
        build_flags: &BTreeMap<String, String>,
        env: &BuildEnv,
    ) -> Result<Option<BuildManifest>> {
        let rune = rune
            .canonicalize()
            .with_context(|| format!("resolve rune {}", rune.display()))?;
        let package_dir = dirs
            .package_dir
            .canonicalize()
            .with_context(|| format!("resolve package dir {}", dirs.package_dir.display()))?;
        let final_prefix = normalize_final_prefix(&dirs.final_prefix);
        let work_dir = dirs
            .work_dir
            .canonicalize()
            .with_context(|| format!("resolve work dir {}", dirs.work_dir.display()))?;

        let host_tool_dir = prepare_host_tool_dir(work_dir.as_path(), &env.host_tools)?;
        let path_entries = build_path_entries(env, host_tool_dir.as_deref());
        let path = build_path_string(&path_entries);
        let sandbox_env = sandbox_env_vars(&work_dir, &env.extra_env)?;
        let context = build_context(
            &package_dir,
            &final_prefix,
            &work_dir,
            sources,
            build_flags,
            path.as_deref(),
            &sandbox_env.context,
            &env.target,
        );
        let env_prefix = path_env_assignment(&path_entries)?;
        let source = format!(
            "{env_prefix}use {} build\nbuild {}\n",
            nuon_string(&rune.display().to_string())?,
            nuon_io::to_nuon_string(&context)?,
        );

        let maybe_value = eval_nu_source(
            &source,
            Some(&format!("grimoire-build-{}", rune.display())),
            package_dir.parent().unwrap_or(&package_dir),
            path.as_deref(),
            &sandbox_env.process,
            dirs.log_file.clone(),
        )?;

        match maybe_value {
            Some(value) => BuildManifest::from_value(value).map(Some),
            None => Ok(None),
        }
    }
}

/// Builds the inert `ctx` record passed to a rune's `build` function. Source paths are the
/// already-fetched, checksum-verified cache locations (AGENTS.md §5.1).
#[allow(clippy::too_many_arguments)]
fn build_context(
    package_dir: &Path,
    final_prefix: &Path,
    work_dir: &Path,
    sources: &BTreeMap<String, FetchedSource>,
    build_flags: &BTreeMap<String, String>,
    path: Option<&str>,
    extra_env: &[(String, String)],
    target: &str,
) -> Value {
    let span = Span::unknown();
    let mut ctx = Record::new();
    ctx.push("package_dir", path_value(package_dir));
    ctx.push("work_dir", path_value(work_dir));
    // `prefix` is the final intended location the package should bake into configure-time
    // metadata. `package_dir` remains the staging root used as DESTDIR.
    ctx.push("prefix", path_value(final_prefix));
    ctx.push("store_path", path_value(final_prefix));
    ctx.push("target", Value::string(target, span));

    let mut sources_record = Record::new();
    for (name, source) in sources {
        let mut entry = Record::new();
        entry.push("path", path_value(&source.path));
        match &source.extracted_dir {
            Some(dir) => entry.push("dir", path_value(dir)),
            None => entry.push("dir", Value::nothing(span)),
        }
        entry.push("url", Value::string(&source.url, span));
        entry.push("sha256", Value::string(&source.sha256, span));
        sources_record.push(name, Value::record(entry, span));
    }
    ctx.push("sources", Value::record(sources_record, span));

    let mut flags = Record::new();
    for (name, value) in build_flags {
        flags.push(name, Value::string(value, span));
    }
    ctx.push("build_flags", Value::record(flags, span));

    ctx.push(
        "nproc",
        Value::int(
            std::thread::available_parallelism()
                .map(|n| n.get() as i64)
                .unwrap_or(4),
            span,
        ),
    );

    let mut env = Record::new();
    if let Some(path) = path {
        env.push("PATH", Value::string(path, span));
    }
    env.push(
        "GRIMOIRE_VERBOSITY",
        Value::string(progress::verbosity_name(), span),
    );
    for (key, value) in extra_env {
        env.push(key, Value::string(value, span));
    }
    ctx.push("env", Value::record(env, span));

    Value::record(ctx, span)
}

fn path_value(path: &Path) -> Value {
    Value::string(path.display().to_string(), Span::unknown())
}

#[derive(Debug)]
struct BuildSandboxEnv {
    /// Values exposed to runes through `ctx.env`.
    context: Vec<(String, String)>,
    /// Values installed into Nushell's execution environment for external build commands.
    process: Vec<(String, String)>,
}

fn sandbox_env_vars(work_dir: &Path, extra_env: &[(String, String)]) -> Result<BuildSandboxEnv> {
    let home = work_dir.join(".grimoire-home");
    let tmp = work_dir.join(".grimoire-tmp");
    let xdg_cache = work_dir.join(".grimoire-xdg").join("cache");
    let xdg_config = work_dir.join(".grimoire-xdg").join("config");
    let xdg_data = work_dir.join(".grimoire-xdg").join("data");
    fs::create_dir_all(&home).with_context(|| format!("create sandbox HOME {}", home.display()))?;
    fs::create_dir_all(&tmp).with_context(|| format!("create sandbox TMPDIR {}", tmp.display()))?;
    fs::create_dir_all(&xdg_cache)
        .with_context(|| format!("create sandbox XDG cache {}", xdg_cache.display()))?;
    fs::create_dir_all(&xdg_config)
        .with_context(|| format!("create sandbox XDG config {}", xdg_config.display()))?;
    fs::create_dir_all(&xdg_data)
        .with_context(|| format!("create sandbox XDG data {}", xdg_data.display()))?;

    let mut context = Vec::new();
    set_env_value(&mut context, "HOME", home.display().to_string());
    set_env_value(&mut context, "TMPDIR", tmp.display().to_string());
    set_env_value(&mut context, "TEMP", tmp.display().to_string());
    set_env_value(&mut context, "TMP", tmp.display().to_string());
    set_env_value(
        &mut context,
        "XDG_CACHE_HOME",
        xdg_cache.display().to_string(),
    );
    set_env_value(
        &mut context,
        "XDG_CONFIG_HOME",
        xdg_config.display().to_string(),
    );
    set_env_value(
        &mut context,
        "XDG_DATA_HOME",
        xdg_data.display().to_string(),
    );
    set_env_value(&mut context, "GRIMOIRE_SANDBOX", "managed-env".to_string());
    set_env_value(
        &mut context,
        "CMAKE_IGNORE_PREFIX_PATH",
        "/opt/homebrew;/usr/local;/opt/local".to_string(),
    );
    set_env_value(
        &mut context,
        "CMAKE_SYSTEM_IGNORE_PREFIX_PATH",
        "/opt/homebrew;/usr/local;/opt/local".to_string(),
    );

    for key in SCRUBBED_DISCOVERY_ENV {
        set_env_value(&mut context, key, String::new());
    }
    for (key, value) in extra_env {
        set_env_value(&mut context, key, value.clone());
    }

    let mut process = blank_inherited_env();
    for (key, value) in &context {
        set_env_value(&mut process, key, value.clone());
    }
    Ok(BuildSandboxEnv { context, process })
}

fn set_env_value(env: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = env.iter_mut().find(|(name, _)| name == key) {
        *existing = value;
    } else {
        env.push((key.to_string(), value));
    }
}

fn blank_inherited_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| !PRESERVED_ENGINE_ENV.contains(&key.as_str()))
        .map(|(key, _)| (key, String::new()))
        .collect()
}

const PRESERVED_ENGINE_ENV: &[&str] = &["PATH", "PWD"];

const SCRUBBED_DISCOVERY_ENV: &[&str] = &[
    "ACLOCAL_PATH",
    "C_INCLUDE_PATH",
    "CMAKE_APPBUNDLE_PATH",
    "CMAKE_FRAMEWORK_PATH",
    "CMAKE_INCLUDE_PATH",
    "CMAKE_LIBRARY_PATH",
    "CMAKE_PREFIX_PATH",
    "CMAKE_PROGRAM_PATH",
    "CPLUS_INCLUDE_PATH",
    "CARGO_HOME",
    "CPATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "GEM_HOME",
    "GEM_PATH",
    "GOPATH",
    "HOMEBREW_CELLAR",
    "HOMEBREW_PREFIX",
    "HOMEBREW_REPOSITORY",
    "LD_LIBRARY_PATH",
    "LIBRARY_PATH",
    "NODE_PATH",
    "NPM_CONFIG_PREFIX",
    "PERL5LIB",
    "PKG_CONFIG_LIBDIR",
    "PKG_CONFIG_PATH",
    "PYTHONHOME",
    "PYTHONPATH",
    "RUBYLIB",
    "RUSTUP_HOME",
];

fn normalize_final_prefix(path: &Path) -> PathBuf {
    // Normalize `.` and `..` without resolving symlinks, so the prefix stays
    // consistent even when the install root contains symlinks.
    path.components().collect()
}

fn exported_const(path: &Path, name: &str) -> Result<Value> {
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

fn eval_nu_source(
    source: &str,
    source_name: Option<&str>,
    cwd: &Path,
    path: Option<&str>,
    extra_env: &[(String, String)],
    log_path: Option<PathBuf>,
) -> Result<Option<Value>> {
    let _working_dir = WorkingDirectoryGuard::enter(cwd)?;
    let mut engine_state =
        nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
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

    if outcome.is_ok() {
        if let Some(path) = &log_path {
            let _ = fs::remove_file(path);
        }
    }

    outcome.map_err(|error| build_failure(&error, &tail, log_path.as_deref()))?;
    Ok(last_value)
}

/// Turns a build-time [`ShellError`] into a reportable error: a one-line Nushell diagnostic
/// (exit code / signal where we have it, otherwise the error's own message) followed by the last
/// few lines of the build's own stdout/stderr. The full log is written to a file and its path is
/// included so the user can inspect the complete output.
fn build_failure(error: &ShellError, tail: &[String], log_path: Option<&Path>) -> anyhow::Error {
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
fn describe_shell_error(error: &ShellError) -> String {
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
const BUILD_TAIL_LINES: usize = 100;

/// Shared ring buffer of the most recent build-output lines, filled by the reader threads
/// regardless of verbosity so a failed build can report what went wrong.
type BuildTail = Arc<Mutex<VecDeque<String>>>;

struct BuildLogStreamer {
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
        if let Some(handle) = self.stdout.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr.take() {
            let _ = handle.join();
        }
        let tail = match self.tail.lock() {
            Ok(tail) => tail.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        };
        (tail, self.log_path.clone())
    }
}

impl Drop for BuildLogStreamer {
    fn drop(&mut self) {
        if let Some(handle) = self.stdout.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_build_log_reader(
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
            if let Some(file) = &log_file {
                if let Ok(mut f) = file.lock() {
                    let _ = writeln!(f, "{}", line);
                    let _ = f.flush();
                }
            }
        }
    })
}

fn pipe_writer_file(writer: os_pipe::PipeWriter) -> std::fs::File {
    use std::os::fd::OwnedFd;

    OwnedFd::from(writer).into()
}

struct WorkingDirectoryGuard {
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

/// Renders a string as a NUON string literal so it can be safely interpolated into the
/// generated Nushell build runner. Routed through `nuon_io` per the single-NUON-layer rule.
fn nuon_string(value: &str) -> Result<String> {
    nuon_io::to_nuon_string(&Value::string(value, nu_protocol::Span::unknown()))
}

/// Directories containing POSIX-mandated utilities that the host OS provides.
/// These are always included in managed build PATH so runes don't need to declare
/// ambient POSIX tools as managed build dependencies.
fn posix_ambient_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
}

fn build_path_entries(env: &BuildEnv, host_tool_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut entries = env.path_dirs.clone();
    if let Some(dir) = host_tool_dir {
        entries.push(dir.to_path_buf());
    }
    // POSIX ambient utilities are always available in managed builds:
    // sed, grep, awk, find, mkdir, cp, chmod, expr, test, etc.
    for dir in posix_ambient_dirs() {
        if dir.is_dir() && !entries.contains(&dir) {
            entries.push(dir);
        }
    }
    if env.inherit_host_path {
        let Some(existing) = std::env::var_os("PATH") else {
            return entries;
        };
        entries.extend(std::env::split_paths(&existing));
    }
    entries
}

fn prepare_host_tool_dir(work_dir: &Path, host_tools: &[HostTool]) -> Result<Option<PathBuf>> {
    if host_tools.is_empty() {
        return Ok(None);
    }

    let dir = work_dir.join(".grimoire-host-tools");
    fs::create_dir_all(&dir).with_context(|| format!("create host tool dir {}", dir.display()))?;
    for tool in host_tools {
        link_host_tool(&dir.join(&tool.name), &tool.path)?;
    }
    Ok(Some(dir))
}

fn link_host_tool(link: &Path, source: &Path) -> Result<()> {
    if link.exists() {
        fs::remove_file(link).with_context(|| format!("replace host tool {}", link.display()))?;
    }
    std::os::unix::fs::symlink(source, link)
        .with_context(|| format!("link host tool {} -> {}", link.display(), source.display()))
}

fn build_path_string(path_entries: &[PathBuf]) -> Option<String> {
    if path_entries.is_empty() {
        return None;
    }
    std::env::join_paths(path_entries)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

fn path_env_assignment(path_entries: &[PathBuf]) -> Result<String> {
    if path_entries.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "$env.PATH = {}\n",
        nuon_io::to_nuon_string(&path_list_value(path_entries))?
    ))
}

fn path_list_value(path_entries: &[PathBuf]) -> Value {
    Value::list(
        path_entries
            .iter()
            .map(|path| path_value(path.as_path()))
            .collect(),
        nu_protocol::Span::unknown(),
    )
}

fn path_value_from_string(path: &str) -> Value {
    let entries = std::env::split_paths(path).collect::<Vec<_>>();
    path_list_value(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_protocol::engine::StateWorkingSet;
    use std::path::Path;

    #[test]
    fn sandbox_env_scrubs_host_discovery_and_allows_managed_overrides() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let managed_pkgconfig = "/grm/store/pkg/lib/pkgconfig";
        let env = sandbox_env_vars(
            temp.path(),
            &[
                ("PKG_CONFIG_PATH".to_string(), managed_pkgconfig.to_string()),
                ("LLVM_PREFIX".to_string(), "/grm/store/llvm".to_string()),
            ],
        )?;

        assert_eq!(
            env_value(&env.context, "PKG_CONFIG_PATH"),
            Some(managed_pkgconfig)
        );
        assert_eq!(
            env_value(&env.context, "LLVM_PREFIX"),
            Some("/grm/store/llvm")
        );
        assert_eq!(env_value(&env.context, "PYTHONPATH"), Some(""));
        assert_eq!(env_value(&env.context, "HOMEBREW_PREFIX"), Some(""));
        assert!(
            env_value(&env.context, "CMAKE_IGNORE_PREFIX_PATH")
                .is_some_and(|value| value.contains("/opt/homebrew"))
        );
        assert_eq!(
            env_value(&env.process, "PKG_CONFIG_PATH"),
            Some(managed_pkgconfig)
        );
        assert_eq!(env_value(&env.process, "PYTHONPATH"), Some(""));
        if let Some((parent_key, _)) =
            std::env::vars().find(|(key, _)| !PRESERVED_ENGINE_ENV.contains(&key.as_str()))
        {
            assert_eq!(env_value(&env.process, &parent_key), Some(""));
        }

        let home = PathBuf::from(env_value(&env.context, "HOME").context("sandbox HOME")?);
        let tmp = PathBuf::from(env_value(&env.context, "TMPDIR").context("sandbox TMPDIR")?);
        let xdg_cache =
            PathBuf::from(env_value(&env.context, "XDG_CACHE_HOME").context("sandbox cache")?);
        assert!(home.starts_with(temp.path()));
        assert!(home.is_dir());
        assert!(tmp.starts_with(temp.path()));
        assert!(tmp.is_dir());
        assert!(xdg_cache.starts_with(temp.path()));
        assert!(xdg_cache.is_dir());
        Ok(())
    }

    fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

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

    #[test]
    fn reads_package_metadata_from_rune() -> Result<()> {
        let runtime = EmbeddedNuRuntime;
        let metadata = runtime.package_metadata(Path::new("tome-example/runes/hello.rn"))?;

        assert_eq!(metadata.name, "hello");
        assert_eq!(metadata.version, "0.1.0");
        // bins are discovered at build time, not declared statically
        assert!(metadata.bins_for("linux-x86_64-musl").is_empty());
        Ok(())
    }
}
