//! Filesystem commands for the rune engine: the documented subset (docs/rune-authoring.md),
//! not nushell's full surface. `open` is always raw — rune data flows through `nuon_io` on
//! the Rust side, never through `from`-style parsing inside a build.

// ShellError is nushell's error type and large by its design; boxing it everywhere would
// diverge from the upstream Command trait shapes this module mirrors.
#![allow(clippy::result_large_err)]

use nu_engine::command_prelude::*;
use nu_protocol::shell_error::{generic::GenericError, io::IoError};
use std::{fs, io::Write, path::PathBuf};

fn absolute(
    engine_state: &EngineState,
    stack: &mut Stack,
    path: &str,
) -> Result<PathBuf, ShellError> {
    let cwd = engine_state.cwd(Some(stack))?;
    let path = nu_path::expand_tilde(path);
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.as_std_path().join(path)
    })
}

#[derive(Clone)]
pub struct Mkdir;

impl Command for Mkdir {
    fn name(&self) -> &str {
        "mkdir"
    }

    fn description(&self) -> &str {
        "Creates directories, including missing parents."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::Nothing)])
            .rest("dirs", SyntaxShape::Filepath, "Directories to create.")
            .switch("verbose", "ignored; accepted for compatibility", Some('v'))
            .category(Category::FileSystem)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let dirs = call.rest::<String>(engine_state, stack, 0)?;
        if dirs.is_empty() {
            return Err(ShellError::MissingParameter {
                param_name: "dirs".into(),
                span: call.head,
            });
        }
        for dir in dirs {
            let path = absolute(engine_state, stack, &dir)?;
            fs::create_dir_all(&path).map_err(|err| IoError::new(err, call.head, path))?;
        }
        Ok(PipelineData::empty())
    }
}

#[derive(Clone)]
pub struct Save;

impl Command for Save {
    fn name(&self) -> &str {
        "save"
    }

    fn description(&self) -> &str {
        "Saves the pipeline input to a file, verbatim (no format conversion)."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Any, Type::Nothing)])
            .required("path", SyntaxShape::Filepath, "File to write.")
            .switch("force", "overwrite an existing file", Some('f'))
            .switch("append", "append instead of overwriting", Some('a'))
            .category(Category::FileSystem)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let path: String = call.req(engine_state, stack, 0)?;
        let force = call.has_flag(engine_state, stack, "force")?;
        let append = call.has_flag(engine_state, stack, "append")?;
        let path = absolute(engine_state, stack, &path)?;
        if path.exists() && !force && !append {
            return Err(ShellError::Generic(GenericError::new(
                format!("{} already exists", path.display()),
                "use --force to overwrite or --append to extend",
                call.head,
            )));
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .open(&path)
            .map_err(|err| IoError::new(err, call.head, path.clone()))?;
        match input {
            PipelineData::ByteStream(stream, ..) => stream.write_to(&mut file)?,
            PipelineData::Value(Value::Binary { val, .. }, ..) => file
                .write_all(&val)
                .map_err(|err| IoError::new(err, call.head, path))?,
            PipelineData::Empty => {}
            data => {
                for value in data {
                    let text = value.coerce_into_string()?;
                    file.write_all(text.as_bytes())
                        .map_err(|err| IoError::new(err, call.head, path.clone()))?;
                }
            }
        }
        Ok(PipelineData::empty())
    }
}

#[derive(Clone)]
pub struct Open;

impl Command for Open {
    fn name(&self) -> &str {
        "open"
    }

    fn description(&self) -> &str {
        "Reads a file as text (or binary when not valid UTF-8). Always raw."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::Any)])
            .required("path", SyntaxShape::Filepath, "File to read.")
            .switch("raw", "always set; accepted for compatibility", Some('r'))
            .category(Category::FileSystem)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let path: String = call.req(engine_state, stack, 0)?;
        let path = absolute(engine_state, stack, &path)?;
        let bytes = fs::read(&path).map_err(|err| IoError::new(err, call.head, path))?;
        let value = match String::from_utf8(bytes) {
            Ok(text) => Value::string(text, call.head),
            Err(err) => Value::binary(err.into_bytes(), call.head),
        };
        Ok(value.into_pipeline_data())
    }
}

#[derive(Clone)]
pub struct Rm;

impl Command for Rm {
    fn name(&self) -> &str {
        "rm"
    }

    fn description(&self) -> &str {
        "Removes files and directories."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::Nothing)])
            .rest("paths", SyntaxShape::Filepath, "Paths to remove.")
            .switch(
                "recursive",
                "remove directories and their contents",
                Some('r'),
            )
            .switch("force", "ignore missing paths", Some('f'))
            .category(Category::FileSystem)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let paths = call.rest::<String>(engine_state, stack, 0)?;
        let recursive = call.has_flag(engine_state, stack, "recursive")?;
        let force = call.has_flag(engine_state, stack, "force")?;
        for raw in paths {
            let path = absolute(engine_state, stack, &raw)?;
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) if force => continue,
                Err(err) => return Err(IoError::new(err, call.head, path).into()),
            };
            let result = if metadata.is_dir() {
                if recursive {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_dir(&path)
                }
            } else {
                fs::remove_file(&path)
            };
            result.map_err(|err| IoError::new(err, call.head, path))?;
        }
        Ok(PipelineData::empty())
    }
}

#[derive(Clone)]
pub struct Cp;

impl Command for Cp {
    fn name(&self) -> &str {
        "cp"
    }

    fn description(&self) -> &str {
        "Copies a file or (with --recursive) a directory."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::Nothing)])
            .required("source", SyntaxShape::Filepath, "Source path.")
            .required("destination", SyntaxShape::Filepath, "Destination path.")
            .switch("recursive", "copy directories recursively", Some('r'))
            .category(Category::FileSystem)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let source: String = call.req(engine_state, stack, 0)?;
        let destination: String = call.req(engine_state, stack, 1)?;
        let recursive = call.has_flag(engine_state, stack, "recursive")?;
        let source = absolute(engine_state, stack, &source)?;
        let mut destination = absolute(engine_state, stack, &destination)?;
        if destination.is_dir()
            && let Some(name) = source.file_name()
        {
            destination = destination.join(name);
        }
        if source.is_dir() {
            if !recursive {
                return Err(ShellError::Generic(GenericError::new(
                    format!("{} is a directory", source.display()),
                    "use --recursive to copy directories",
                    call.head,
                )));
            }
            copy_dir_recursive(&source, &destination)
                .map_err(|err| IoError::new(err, call.head, source))?;
        } else {
            fs::copy(&source, &destination).map_err(|err| IoError::new(err, call.head, source))?;
        }
        Ok(PipelineData::empty())
    }
}

fn copy_dir_recursive(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else if file_type.is_symlink() {
            let link = fs::read_link(entry.path())?;
            std::os::unix::fs::symlink(link, &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct Ls;

impl Command for Ls {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "Lists directory entries as records with name, type, and size."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::table())])
            .optional(
                "path",
                SyntaxShape::Filepath,
                "Directory to list (default: cwd).",
            )
            .category(Category::FileSystem)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let head = call.head;
        let arg: Option<String> = call.opt(engine_state, stack, 0)?;
        // Like upstream: with an explicit directory argument the names are prefixed with it,
        // without one they are bare entry names in the cwd.
        let (read_root, prefix) = match &arg {
            Some(raw) => (absolute(engine_state, stack, raw)?, Some(raw.clone())),
            None => (engine_state.cwd(Some(stack))?.into_std_path_buf(), None),
        };
        let mut rows = Vec::new();
        let entries =
            fs::read_dir(&read_root).map_err(|err| IoError::new(err, head, read_root.clone()))?;
        for entry in entries {
            let entry = entry.map_err(|err| IoError::new(err, head, read_root.clone()))?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let name = match &prefix {
                Some(prefix) => PathBuf::from(prefix).join(&file_name).display().to_string(),
                None => file_name,
            };
            let file_type = entry
                .file_type()
                .map_err(|err| IoError::new(err, head, entry.path()))?;
            let kind = if file_type.is_dir() {
                "dir"
            } else if file_type.is_symlink() {
                "symlink"
            } else {
                "file"
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            rows.push(Value::record(
                nu_protocol::record! {
                    "name" => Value::string(name, head),
                    "type" => Value::string(kind, head),
                    "size" => Value::filesize(size as i64, head),
                },
                head,
            ));
        }
        rows.sort_by_key(|row| {
            row.get_data_by_key("name")
                .and_then(|v| v.coerce_into_string().ok())
                .unwrap_or_default()
        });
        Ok(Value::list(rows, head).into_pipeline_data())
    }
}

#[derive(Clone)]
pub struct Cd;

impl Command for Cd {
    fn name(&self) -> &str {
        "cd"
    }

    fn description(&self) -> &str {
        "Changes the working directory."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::Nothing)])
            .required("path", SyntaxShape::Directory, "Directory to change to.")
            .category(Category::FileSystem)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let path: String = call.req(engine_state, stack, 0)?;
        let path = absolute(engine_state, stack, &path)?;
        if !path.is_dir() {
            return Err(ShellError::Generic(GenericError::new(
                format!("{} is not a directory", path.display()),
                "cd target must be an existing directory",
                call.head,
            )));
        }
        stack.set_cwd(path)?;
        Ok(PipelineData::empty())
    }
}
