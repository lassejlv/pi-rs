//! Port of `packages/coding-agent/src/modes/interactive/external-editor.ts`.

use crate::utils::text::strip_bom;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalEditorOptions {
    pub command: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all_fields = "camelCase")]
pub enum ExternalEditorResult {
    #[serde(rename = "complete")]
    Complete { content: String },
    #[serde(rename = "failed")]
    Failed {},
}

/// `fs.mkdtempSync(prefix)`: a new private (0700) directory named `<prefix>XXXXXX`.
pub(crate) fn make_temp_dir(prefix: &str) -> std::io::Result<PathBuf> {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    for _ in 0..100 {
        let bytes = uuid::Uuid::new_v4();
        let suffix: String = bytes.as_bytes()[..6].iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect();
        let path = std::env::temp_dir().join(format!("{prefix}{suffix}"));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("could not create a temporary directory"))
}

pub async fn edit_in_external_editor(options: &ExternalEditorOptions) -> anyhow::Result<ExternalEditorResult> {
    let directory = make_temp_dir("pi-editor-")?;
    let file_path = directory.join("prompt.md");
    let result = run_editor(options, &file_path).await;
    // Cleanup is best effort.
    let _ = std::fs::remove_dir_all(&directory);
    result
}

async fn run_editor(options: &ExternalEditorOptions, file_path: &Path) -> anyhow::Result<ExternalEditorResult> {
    std::fs::write(file_path, &options.content)?;
    let mut parts = options.command.split(' ');
    let editor = parts.next().unwrap_or_default().to_string();
    let editor_args: Vec<String> = parts.map(str::to_string).collect();
    {
        let mut stdout = std::io::stdout();
        let _ = write!(
            stdout,
            "Launching external editor: {}\nPi will resume when the editor exits.\n",
            options.command
        );
        let _ = stdout.flush();
    }

    let mut command = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(&editor);
        c
    } else {
        tokio::process::Command::new(&editor)
    };
    command
        .args(&editor_args)
        .arg(file_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    let exit_code = match command.status().await {
        Ok(status) => status.code(),
        Err(_) => None,
    };
    if exit_code != Some(0) {
        return Ok(ExternalEditorResult::Failed {});
    }
    let content = strip_bom(&std::fs::read_to_string(file_path)?);
    let content = content.strip_suffix('\n').unwrap_or(&content).to_string();
    Ok(ExternalEditorResult::Complete { content })
}

#[cfg(all(test, unix))]
mod tests {
    //! Port of `test/external-editor.test.ts`. The fake editor is a shell script instead of a Node script.
    use super::*;

    const FAKE_EDITOR: &str = r#"#!/bin/sh
capture="$1"
for last; do :; done
file="$last"
dir=$(dirname "$file")
{
  echo "$file"
  cat "$file"; echo
  ls -A "$dir" | tr '\n' ' '; echo
  stat -c %a "$dir" 2>/dev/null || stat -f %Lp "$dir"
} > "$capture"
for arg; do
  [ "$arg" = "--fail" ] && exit 1
done
for arg; do
  if [ "$arg" = "--empty" ]; then : > "$file"; exit 0; fi
done
printf 'edited\n' > "$file"
"#;

    struct Capture {
        file_path: PathBuf,
        content: String,
        entries: Vec<String>,
        directory_mode: u32,
    }

    async fn run_external_editor(flag: Option<&str>) -> (ExternalEditorResult, Capture) {
        let test_dir = make_temp_dir("pi-external-editor-test-").unwrap();
        let script = test_dir.join("fake-editor.sh");
        std::fs::write(&script, FAKE_EDITOR).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let capture_path = test_dir.join("capture.txt");
        let command = format!(
            "{} {}{}",
            script.display(),
            capture_path.display(),
            flag.map(|f| format!(" {f}")).unwrap_or_default()
        );
        let result = edit_in_external_editor(&ExternalEditorOptions { command, content: "original".into() })
            .await
            .unwrap();
        let raw = std::fs::read_to_string(&capture_path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        let capture = Capture {
            file_path: PathBuf::from(lines[0]),
            content: lines[1].to_string(),
            entries: lines[2].split_whitespace().map(str::to_string).collect(),
            directory_mode: u32::from_str_radix(lines[3].trim(), 8).unwrap(),
        };
        let _ = std::fs::remove_dir_all(&test_dir);
        (result, capture)
    }

    #[tokio::test]
    async fn edits_a_prompt_inside_a_private_temporary_directory() {
        let (result, capture) = run_external_editor(None).await;
        let directory = capture.file_path.parent().unwrap().to_path_buf();
        assert_eq!(result, ExternalEditorResult::Complete { content: "edited".into() });
        assert_eq!(directory.parent().unwrap(), std::env::temp_dir());
        let dir_name = directory.file_name().unwrap().to_string_lossy().to_string();
        assert!(dir_name.starts_with("pi-editor-") && dir_name.len() > "pi-editor-".len());
        assert_eq!(capture.file_path.file_name().unwrap(), "prompt.md");
        assert_eq!(capture.entries, vec!["prompt.md".to_string()]);
        assert_eq!(capture.content, "original");
        assert_eq!(capture.directory_mode & 0o077, 0);
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn keeps_the_original_content_when_the_editor_exits_unsuccessfully() {
        let (result, capture) = run_external_editor(Some("--fail")).await;
        assert_eq!(result, ExternalEditorResult::Failed {});
        assert!(!capture.file_path.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn returns_empty_content_when_the_editor_clears_the_prompt() {
        let (result, _) = run_external_editor(Some("--empty")).await;
        assert_eq!(result, ExternalEditorResult::Complete { content: String::new() });
    }
}
