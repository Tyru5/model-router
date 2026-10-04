use crate::config::{CustomAdapter, Mode, Stage};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputKind {
    Text,
    Codex,
    Claude,
    Pi,
    Cursor,
    Gemini,
    OpenCode,
    Amp,
}

#[derive(Debug, Clone)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub kind: OutputKind,
    pub final_file: PathBuf,
}
impl Invocation {
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(|arg| shell_words::quote(arg).into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub fn executable(program: &str) -> Option<PathBuf> {
    fn usable(path: &Path) -> bool {
        if !path.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
    let path = Path::new(program);
    if path.components().count() > 1 {
        return usable(path).then(|| path.to_path_buf());
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(program))
        .find(|p| usable(p))
}

pub fn executable_at(program: &str, project: &Path) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.is_relative() && path.components().count() > 1 {
        return executable(project.join(path).to_str()?);
    }
    executable(program)
}

pub fn program(harness: &str, custom: &BTreeMap<String, CustomAdapter>) -> String {
    if let Some(adapter) = custom.get(harness) {
        return adapter.command.clone();
    }
    match harness {
        "cursor" if executable("cursor-agent").is_some() => "cursor-agent".into(),
        "cursor" => "agent".into(),
        other => other.into(),
    }
}

pub fn validate_options(stage: &Stage) -> Result<()> {
    if !stage.provider.is_empty()
        && matches!(
            stage.harness.as_str(),
            "claude" | "cursor" | "gemini" | "amp" | "copilot"
        )
    {
        bail!(
            "{} uses its existing provider/auth configuration; leave the provider field empty",
            stage.harness
        );
    }
    if !stage.provider.is_empty() {
        ensure!(
            !stage.model.is_empty(),
            "A provider override requires a model"
        );
    }
    if !stage.effort.is_empty()
        && matches!(
            stage.harness.as_str(),
            "cursor" | "gemini" | "aider" | "goose" | "amp" | "copilot"
        )
    {
        bail!(
            "{} has no portable effort flag; use its native model/mode or explicit arguments instead",
            stage.harness
        );
    }
    Ok(())
}

pub struct ContextPaths<'a> {
    pub workspace: &'a Path,
    pub attempt_dir: &'a Path,
    pub prompt_file: &'a Path,
    pub handoff_file: &'a Path,
}

pub fn build(
    stage: &Stage,
    custom: &BTreeMap<String, CustomAdapter>,
    prompt: &str,
    paths: &ContextPaths<'_>,
) -> Result<Invocation> {
    validate_options(stage)?;
    let interactive = stage.mode == Mode::Interactive;
    let mut invocation = Invocation {
        program: program(&stage.harness, custom),
        args: vec![],
        stdin: None,
        kind: OutputKind::Text,
        final_file: paths.attempt_dir.join("final.txt"),
    };
    let a = &mut invocation.args;
    let mut append = |items: &[&str]| {
        a.extend(items.iter().map(|s| (*s).to_owned()));
    };
    let qualified = if stage.provider.is_empty() {
        stage.model.clone()
    } else {
        format!("{}/{}", stage.provider, stage.model)
    };
    match stage.harness.as_str() {
        "codex" => {
            invocation.kind = OutputKind::Codex;
            if !interactive {
                append(&[
                    "exec",
                    "--json",
                    "--color",
                    "never",
                    "--skip-git-repo-check",
                    "--output-last-message",
                    invocation
                        .final_file
                        .to_str()
                        .context("Output path must be UTF-8")?,
                ]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if !stage.provider.is_empty() {
                if matches!(stage.provider.as_str(), "ollama" | "lmstudio") {
                    append(&["--oss", "--local-provider", &stage.provider]);
                } else {
                    append(&[
                        "--config",
                        &format!("model_provider={}", serde_json::to_string(&stage.provider)?),
                    ]);
                }
            }
            if !stage.effort.is_empty() {
                append(&[
                    "--config",
                    &format!(
                        "model_reasoning_effort={}",
                        serde_json::to_string(&stage.effort)?
                    ),
                ]);
            }
            a.extend(stage.extra_args.clone());
            if interactive {
                a.push(prompt.into());
            } else {
                a.push("-".into());
                invocation.stdin = Some(prompt.into());
            }
        }
        "claude" => {
            invocation.kind = OutputKind::Claude;
            if !interactive {
                append(&["--print", "--output-format", "json"]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if !stage.effort.is_empty() {
                append(&["--effort", &stage.effort]);
            }
            a.extend(stage.extra_args.clone());
            if interactive {
                a.push(prompt.into());
            } else {
                invocation.stdin = Some(prompt.into());
            }
        }
        "pi" => {
            invocation.kind = OutputKind::Pi;
            if !interactive {
                append(&["--print", "--mode", "json"]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if !stage.provider.is_empty() {
                append(&["--provider", &stage.provider]);
            }
            if !stage.effort.is_empty() {
                append(&["--thinking", &stage.effort]);
            }
            a.extend(stage.extra_args.clone());
            a.push(format!("@{}", paths.prompt_file.display()));
        }
        "cursor" => {
            invocation.kind = OutputKind::Cursor;
            if !interactive {
                append(&["--print", "--output-format", "json"]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            a.extend(stage.extra_args.clone());
            a.push(prompt.into());
        }
        "gemini" => {
            invocation.kind = OutputKind::Gemini;
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if interactive {
                append(&["--prompt-interactive", prompt]);
            } else {
                append(&["--prompt", prompt, "--output-format", "json"]);
            }
            a.extend(stage.extra_args.clone());
        }
        "opencode" => {
            invocation.kind = OutputKind::OpenCode;
            if interactive {
                append(&["run", "--interactive"]);
            } else {
                append(&["run", "--format", "json"]);
            }
            if !qualified.is_empty() {
                append(&["--model", &qualified]);
            }
            if !stage.effort.is_empty() {
                append(&["--variant", &stage.effort]);
            }
            a.extend(stage.extra_args.clone());
            a.push(prompt.into());
        }
        "aider" => {
            if !qualified.is_empty() {
                append(&["--model", &qualified]);
            }
            append(&["--no-auto-commits", "--no-dirty-commits"]);
            if interactive {
                let commands = paths.attempt_dir.join("aider.commands");
                fs::write(
                    &commands,
                    "/code Complete the stage described in the attached model-router prompt file.\n",
                )?;
                append(&[
                    "--read",
                    paths
                        .prompt_file
                        .to_str()
                        .context("Prompt path must be UTF-8")?,
                    "--load",
                    commands.to_str().context("Command path must be UTF-8")?,
                ]);
            } else {
                append(&[
                    "--message-file",
                    paths
                        .prompt_file
                        .to_str()
                        .context("Prompt path must be UTF-8")?,
                ]);
            }
            a.extend(stage.extra_args.clone());
        }
        "goose" => {
            append(&["run", "--text", prompt]);
            if interactive {
                append(&["--interactive"]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if !stage.provider.is_empty() {
                append(&["--provider", &stage.provider]);
            }
            a.extend(stage.extra_args.clone());
        }
        "amp" => {
            invocation.kind = OutputKind::Amp;
            if !stage.model.is_empty() {
                append(&["--mode", &stage.model]);
            }
            if !interactive {
                append(&["--execute", "--stream-json"]);
            }
            a.extend(stage.extra_args.clone());
            if interactive {
                a.push(prompt.into());
            } else {
                invocation.stdin = Some(prompt.into());
            }
        }
        "copilot" => {
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            append(&[
                if interactive {
                    "--interactive"
                } else {
                    "--prompt"
                },
                prompt,
            ]);
            a.extend(stage.extra_args.clone());
        }
        name => {
            let adapter = custom
                .get(name)
                .with_context(|| format!("Unknown adapter {name}"))?;
            let args = if interactive {
                adapter
                    .interactive_args
                    .as_ref()
                    .with_context(|| format!("{name} does not define interactive_args"))?
            } else {
                &adapter.args
            };
            let values = [
                ("{prompt}", prompt.to_owned()),
                ("{model}", stage.model.clone()),
                ("{provider}", stage.provider.clone()),
                ("{effort}", stage.effort.clone()),
                ("{prompt_file}", paths.prompt_file.display().to_string()),
                ("{handoff_file}", paths.handoff_file.display().to_string()),
                ("{workspace}", paths.workspace.display().to_string()),
                ("{run_dir}", paths.attempt_dir.display().to_string()),
            ];
            a.extend(args.iter().map(|arg| expand(arg, &values)));
            a.extend(stage.extra_args.clone());
            if adapter.prompt_stdin && !interactive {
                invocation.stdin = Some(prompt.into());
            }
        }
    }
    Ok(invocation)
}

/// Expand only tokens present in the template, never tokens introduced by user input.
fn expand(template: &str, values: &[(&str, String)]) -> String {
    let mut output = String::new();
    let mut rest = template;
    while !rest.is_empty() {
        if let Some((key, value)) = values.iter().find(|(key, _)| rest.starts_with(*key)) {
            output.push_str(value);
            rest = &rest[key.len()..];
        } else {
            let c = rest.chars().next().unwrap();
            output.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn templates_do_not_recursively_interpolate_prompt() {
        assert_eq!(
            expand(
                "x {prompt} {model}",
                &[
                    ("{prompt}", "literal {model} $(touch x)".into()),
                    ("{model}", "chosen".into())
                ]
            ),
            "x literal {model} $(touch x) chosen"
        );
    }
    #[test]
    fn amp_uses_mode_not_model() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ContextPaths {
            workspace: dir.path(),
            attempt_dir: dir.path(),
            prompt_file: dir.path(),
            handoff_file: dir.path(),
        };
        let i = build(
            &Stage {
                harness: "amp".into(),
                model: "high".into(),
                ..Stage::default()
            },
            &BTreeMap::new(),
            "task",
            &paths,
        )
        .unwrap();
        assert!(i.args.windows(2).any(|a| a == ["--mode", "high"]));
        assert!(!i.args.iter().any(|a| a == "--model"));
    }
}
