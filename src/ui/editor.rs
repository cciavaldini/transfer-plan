use rustyline::completion::FilenameCompleter;
use rustyline::history::DefaultHistory;
use rustyline::{CompletionType, Config, Editor, EditMode};
use rustyline::{Completer, Helper, Highlighter, Hinter, Validator};
// (no unused imports)
use anyhow::Result;
use std::path::{Path, PathBuf};

// Custom helper that combines filename completion
#[derive(Helper, Completer, Hinter, Validator, Highlighter)]
pub struct PathCompleter {
    #[rustyline(Completer)]
    completer: FilenameCompleter,
}

impl PathCompleter {
    pub fn new() -> Self {
        Self {
            completer: FilenameCompleter::new(),
        }
    }
}

pub fn new_path_editor() -> Result<Editor<PathCompleter, DefaultHistory>> {
    #[cfg(unix)]
    let completion_type = CompletionType::Fuzzy;
    #[cfg(not(unix))]
    let completion_type = CompletionType::List;

    let config = Config::builder()
        .completion_type(completion_type)
        .edit_mode(EditMode::Emacs)
        .build();
    let mut editor = Editor::with_config(config)?;
    editor.set_helper(Some(PathCompleter::new()));
    Ok(editor)
}

pub fn resolve_user_path(input: &str, default_base: &Path) -> PathBuf {
    let expanded = shellexpand::tilde(input);
    let path = PathBuf::from(expanded.to_string());
    if path.is_relative() {
        default_base.join(path)
    } else {
        path
    }
}

pub fn validate_source_path(source: &Path) -> Result<()> {
    if !source.exists() {
        anyhow::bail!("Path does not exist: {}", source.display());
    }

    if source.is_file() {
        std::fs::File::open(source)
            .map_err(|e| anyhow::anyhow!("Cannot read file (permission denied): {}", e))?;
    }

    Ok(())
}
