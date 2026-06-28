//! Path, string, and filter commands for the rune engine — the documented subset
//! (docs/rune-authoring.md). Path/str commands operate on string input; filters operate on
//! the documented record/list shapes.

// ShellError is nushell's error type and large by its design; boxing it everywhere would
// diverge from the upstream Command trait shapes this module mirrors.
#![allow(clippy::result_large_err)]

use nu_engine::command_prelude::*;
use nu_protocol::shell_error::generic::GenericError;
use std::path::{Path, PathBuf};

fn input_string(input: PipelineData, span: Span) -> Result<(String, Span), ShellError> {
    let value = input.into_value(span)?;
    let value_span = value.span();
    Ok((value.coerce_into_string()?, value_span))
}

macro_rules! simple_command {
    ($struct:ident, $name:expr, $desc:expr, $sig:expr, $run:expr) => {
        #[derive(Clone)]
        pub struct $struct;

        impl Command for $struct {
            fn name(&self) -> &str {
                $name
            }
            fn description(&self) -> &str {
                $desc
            }
            fn signature(&self) -> Signature {
                $sig
            }
            #[allow(clippy::redundant_closure_call)]
            fn run(
                &self,
                engine_state: &EngineState,
                stack: &mut Stack,
                call: &Call,
                input: PipelineData,
            ) -> Result<PipelineData, ShellError> {
                $run(engine_state, stack, call, input)
            }
        }
    };
}

// --- path ------------------------------------------------------------------

simple_command!(
    PathSelf,
    "path",
    "Path manipulation subcommands.",
    Signature::build("path")
        .input_output_types(vec![(Type::Nothing, Type::String)])
        .category(Category::Path),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     _i: PipelineData|
     -> Result<PipelineData, ShellError> {
        Err(ShellError::Generic(GenericError::new(
            "`path` requires a subcommand",
            "use `path join`, `path exists`, `path type`, `path basename`, or `path dirname`",
            call.head,
        )))
    }
);

simple_command!(
    PathJoin,
    "path join",
    "Joins path components onto the input path.",
    Signature::build("path join")
        .input_output_types(vec![(Type::String, Type::String)])
        .rest("parts", SyntaxShape::String, "Components to append.")
        .category(Category::Path),
    |engine_state: &EngineState,
     stack: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let parts = call.rest::<String>(engine_state, stack, 0)?;
        let (base, _) = input_string(input, call.head)?;
        let mut path = PathBuf::from(base);
        for part in parts {
            path.push(part);
        }
        Ok(Value::string(path.display().to_string(), call.head).into_pipeline_data())
    }
);

simple_command!(
    PathExists,
    "path exists",
    "Whether the input path exists on disk.",
    Signature::build("path exists")
        .input_output_types(vec![(Type::String, Type::Bool)])
        .category(Category::Path),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let (path, _) = input_string(input, call.head)?;
        Ok(Value::bool(Path::new(&path).exists(), call.head).into_pipeline_data())
    }
);

simple_command!(
    PathType,
    "path type",
    "The type of the input path: dir, file, symlink, or null when missing.",
    Signature::build("path type")
        .input_output_types(vec![(Type::String, Type::Any)])
        .category(Category::Path),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let (path, _) = input_string(input, call.head)?;
        let value = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Value::string("symlink", call.head)
            }
            Ok(metadata) if metadata.is_dir() => Value::string("dir", call.head),
            Ok(_) => Value::string("file", call.head),
            Err(_) => Value::nothing(call.head),
        };
        Ok(value.into_pipeline_data())
    }
);

simple_command!(
    PathBasename,
    "path basename",
    "The final component of the input path.",
    Signature::build("path basename")
        .input_output_types(vec![(Type::String, Type::String)])
        .category(Category::Path),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let (path, _) = input_string(input, call.head)?;
        let basename = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Value::string(basename, call.head).into_pipeline_data())
    }
);

simple_command!(
    PathDirname,
    "path dirname",
    "The input path without its final component.",
    Signature::build("path dirname")
        .input_output_types(vec![(Type::String, Type::String)])
        .category(Category::Path),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let (path, _) = input_string(input, call.head)?;
        let dirname = Path::new(&path)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Ok(Value::string(dirname, call.head).into_pipeline_data())
    }
);

// --- str -------------------------------------------------------------------

simple_command!(
    StrSelf,
    "str",
    "String manipulation subcommands.",
    Signature::build("str")
        .input_output_types(vec![(Type::Nothing, Type::String)])
        .category(Category::Strings),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     _i: PipelineData|
     -> Result<PipelineData, ShellError> {
        Err(ShellError::Generic(GenericError::new(
            "`str` requires a subcommand",
            "use `str starts-with`, `str ends-with`, `str contains`, `str trim`, or `str replace`",
            call.head,
        )))
    }
);

simple_command!(
    StrStartsWith,
    "str starts-with",
    "Whether the input string starts with the given prefix.",
    Signature::build("str starts-with")
        .input_output_types(vec![(Type::String, Type::Bool)])
        .required("prefix", SyntaxShape::String, "Prefix to test.")
        .category(Category::Strings),
    |engine_state: &EngineState,
     stack: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let prefix: String = call.req(engine_state, stack, 0)?;
        let (text, _) = input_string(input, call.head)?;
        Ok(Value::bool(text.starts_with(&prefix), call.head).into_pipeline_data())
    }
);

simple_command!(
    StrEndsWith,
    "str ends-with",
    "Whether the input string ends with the given suffix.",
    Signature::build("str ends-with")
        .input_output_types(vec![(Type::String, Type::Bool)])
        .required("suffix", SyntaxShape::String, "Suffix to test.")
        .category(Category::Strings),
    |engine_state: &EngineState,
     stack: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let suffix: String = call.req(engine_state, stack, 0)?;
        let (text, _) = input_string(input, call.head)?;
        Ok(Value::bool(text.ends_with(&suffix), call.head).into_pipeline_data())
    }
);

simple_command!(
    StrContains,
    "str contains",
    "Whether the input string contains the given substring.",
    Signature::build("str contains")
        .input_output_types(vec![(Type::String, Type::Bool)])
        .required("substring", SyntaxShape::String, "Substring to search for.")
        .category(Category::Strings),
    |engine_state: &EngineState,
     stack: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let substring: String = call.req(engine_state, stack, 0)?;
        let (text, _) = input_string(input, call.head)?;
        Ok(Value::bool(text.contains(&substring), call.head).into_pipeline_data())
    }
);

simple_command!(
    StrTrim,
    "str trim",
    "Trims whitespace from both ends of the input string.",
    Signature::build("str trim")
        .input_output_types(vec![(Type::String, Type::String)])
        .category(Category::Strings),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let (text, _) = input_string(input, call.head)?;
        Ok(Value::string(text.trim(), call.head).into_pipeline_data())
    }
);

simple_command!(
    StrReplace,
    "str replace",
    "Replaces the first occurrence (or all with --all) of a pattern in the input string.",
    Signature::build("str replace")
        .input_output_types(vec![(Type::String, Type::String)])
        .required("find", SyntaxShape::String, "Text or regex to find.")
        .required("replace", SyntaxShape::String, "Replacement text.")
        .switch("regex", "interpret the pattern as a regex", Some('r'))
        .switch("all", "replace every occurrence", Some('a'))
        .category(Category::Strings),
    |engine_state: &EngineState,
     stack: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let find: String = call.req(engine_state, &mut *stack, 0)?;
        let replace: String = call.req(engine_state, &mut *stack, 1)?;
        let use_regex = call.has_flag(engine_state, &mut *stack, "regex")?;
        let all = call.has_flag(engine_state, &mut *stack, "all")?;
        let (text, _) = input_string(input, call.head)?;
        let result = if use_regex {
            let re = regex::Regex::new(&find).map_err(|err| {
                ShellError::Generic(GenericError::new(
                    format!("invalid regex: {err}"),
                    "fix the pattern",
                    call.head,
                ))
            })?;
            if all {
                re.replace_all(&text, replace.as_str()).into_owned()
            } else {
                re.replace(&text, replace.as_str()).into_owned()
            }
        } else if all {
            text.replace(&find, &replace)
        } else {
            text.replacen(&find, &replace, 1)
        };
        Ok(Value::string(result, call.head).into_pipeline_data())
    }
);

// --- filters ---------------------------------------------------------------

simple_command!(
    Get,
    "get",
    "Extracts data at a cell path from the input (mapping over list rows).",
    Signature::build("get")
        .input_output_types(vec![(Type::Any, Type::Any)])
        .required("cell_path", SyntaxShape::CellPath, "Cell path to extract.")
        .category(Category::Filters),
    |engine_state: &EngineState,
     stack: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let cell_path: nu_protocol::ast::CellPath = call.req(engine_state, stack, 0)?;
        let value = input.into_value(call.head)?;
        let extracted = value.follow_cell_path(&cell_path.members)?;
        Ok(extracted.into_owned().into_pipeline_data())
    }
);

simple_command!(
    Merge,
    "merge",
    "Merges the given record over the input record.",
    Signature::build("merge")
        .input_output_types(vec![(Type::record(), Type::record())])
        .required("value", SyntaxShape::Any, "Record to merge over the input.")
        .category(Category::Filters),
    |engine_state: &EngineState,
     stack: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let overlay: Value = call.req(engine_state, stack, 0)?;
        let base = input.into_value(call.head)?;
        match (base, overlay) {
            (Value::Record { val: base, .. }, Value::Record { val: overlay, .. }) => {
                let mut merged = base.into_owned();
                for (key, value) in overlay.into_owned() {
                    merged.insert(key, value);
                }
                Ok(Value::record(merged, call.head).into_pipeline_data())
            }
            _ => Err(ShellError::Generic(GenericError::new(
                "merge requires record input and a record argument",
                "both sides must be records",
                call.head,
            ))),
        }
    }
);

simple_command!(
    Columns,
    "columns",
    "The column (field) names of the input record.",
    Signature::build("columns")
        .input_output_types(vec![(Type::record(), Type::list(Type::String))])
        .category(Category::Filters),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let value = input.into_value(call.head)?;
        match value {
            Value::Record { val, .. } => {
                let names = val
                    .columns()
                    .map(|name| Value::string(name, call.head))
                    .collect();
                Ok(Value::list(names, call.head).into_pipeline_data())
            }
            _ => Err(ShellError::Generic(GenericError::new(
                "columns requires record input",
                "input must be a record",
                call.head,
            ))),
        }
    }
);

simple_command!(
    Lines,
    "lines",
    "Splits string input into a list of lines.",
    Signature::build("lines")
        .input_output_types(vec![(Type::String, Type::list(Type::String))])
        .category(Category::Filters),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let (text, _) = input_string(input, call.head)?;
        let lines = text
            .lines()
            .map(|line| Value::string(line, call.head))
            .collect();
        Ok(Value::list(lines, call.head).into_pipeline_data())
    }
);

simple_command!(
    First,
    "first",
    "The first element of the input list.",
    Signature::build("first")
        .input_output_types(vec![(Type::list(Type::Any), Type::Any)])
        .category(Category::Filters),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let value = input.into_value(call.head)?;
        match value {
            Value::List { vals, .. } => vals
                .into_iter()
                .next()
                .map(|v| v.into_pipeline_data())
                .ok_or_else(|| {
                    ShellError::Generic(GenericError::new(
                        "first on an empty list",
                        "the input list has no elements",
                        call.head,
                    ))
                }),
            _ => Err(ShellError::Generic(GenericError::new(
                "first requires list input",
                "input must be a list",
                call.head,
            ))),
        }
    }
);

simple_command!(
    IsEmpty,
    "is-empty",
    "Whether the input string, list, or record is empty.",
    Signature::build("is-empty")
        .input_output_types(vec![(Type::Any, Type::Bool)])
        .category(Category::Filters),
    |_e: &EngineState,
     _s: &mut Stack,
     call: &Call,
     input: PipelineData|
     -> Result<PipelineData, ShellError> {
        let value = input.into_value(call.head)?;
        let empty = match value {
            Value::String { val, .. } => val.is_empty(),
            Value::List { vals, .. } => vals.is_empty(),
            Value::Record { val, .. } => val.is_empty(),
            Value::Nothing { .. } => true,
            _ => false,
        };
        Ok(Value::bool(empty, call.head).into_pipeline_data())
    }
);
