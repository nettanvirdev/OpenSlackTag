//! Skills: folders of instructions for specific jobs, in the same format as
//! Claude's Agent Skills (`<skills dir>/<skill>/SKILL.md` with `name` and
//! `description` frontmatter). Only names and descriptions go in the system
//! prompt; the teammate reads a skill when a task calls for it.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use openslacktag_core::error::{Error, Result as ToolResult};
use openslacktag_core::tool::{PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource};
use serde_json::{Value, json};

const MAX_DESCRIPTION_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// The folder name, used to read it.
    pub id: String,
    pub name: String,
    pub description: String,
    dir: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Skills {
    skills: Arc<Vec<Skill>>,
}

impl Skills {
    /// Every skill folder under `dir`. A missing folder means no skills.
    pub fn load(dir: &Path) -> Result<Self> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", dir.display())),
        };
        let mut skills = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let file = path.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let id = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (fields, _) = frontmatter(&text);
            let get = |key: &str| {
                fields
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
            };
            skills.push(Skill {
                name: get("name").unwrap_or_else(|| id.clone()),
                description: get("description")
                    .map(|d| d.chars().take(MAX_DESCRIPTION_CHARS).collect())
                    .unwrap_or_else(|| "No description; read it only if someone names it.".into()),
                id,
                dir: path,
            });
        }
        skills.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Self {
            skills: Arc::new(skills),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// The lines for the system prompt, or `None` with no skills.
    pub fn listing(&self) -> Option<String> {
        (!self.is_empty()).then(|| {
            self.skills
                .iter()
                .map(|s| format!("- {} (`{}`): {}", s.name, s.id, s.description))
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        if self.is_empty() {
            return Vec::new();
        }
        vec![Arc::new(SkillRead {
            skills: self.clone(),
        })]
    }

    fn read(&self, id: &str, file: Option<&str>) -> ToolResult<String> {
        let skill = self
            .skills
            .iter()
            .find(|s| s.id == id || s.name.eq_ignore_ascii_case(id))
            .ok_or_else(|| Error::not_found("skill", id))?;
        let relative = Path::new(file.unwrap_or("SKILL.md"));
        // Only plain relative paths inside the skill's own folder.
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(Error::InvalidInput(
                "file must be a path inside the skill folder".into(),
            ));
        }
        let text = std::fs::read_to_string(skill.dir.join(relative))
            .map_err(|_| Error::not_found("skill file", relative.display().to_string()))?;
        if file.is_some() {
            return Ok(text);
        }
        let (_, body) = frontmatter(&text);
        let mut others = Vec::new();
        list_files(&skill.dir, &skill.dir, &mut others);
        others.retain(|f| f != "SKILL.md");
        Ok(if others.is_empty() {
            body.to_string()
        } else {
            format!(
                "{body}\n\n(Other files in this skill, readable with skill_read and `file`: {})",
                others.join(", ")
            )
        })
    }
}

/// `key: value` pairs between leading `---` lines, and the body after them.
fn frontmatter(text: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (Vec::new(), text);
    };
    let Some(end) = rest.find("\n---") else {
        return (Vec::new(), text);
    };
    let fields = rest[..end]
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let value = value.trim().trim_matches('"').trim_matches('\'');
            Some((key.trim().to_string(), value.to_string()))
        })
        .collect();
    let body = rest[end + 4..].trim_start_matches(['-', '\r', '\n']);
    (fields, body)
}

fn list_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            list_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out.sort();
}

#[derive(Debug)]
struct SkillRead {
    skills: Skills,
}

#[async_trait]
impl Tool for SkillRead {
    fn id(&self) -> &str {
        "skill_read"
    }

    fn description(&self) -> &str {
        "Read a skill: instructions for a specific kind of task, listed in your system prompt. Read it before doing \
         a task it covers. Pass `file` to read another file the skill mentions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The skill's id (in backticks in the list)"},
                "file": {"type": "string", "description": "Optional: another file in the skill's folder"}
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    fn permission(&self, args: &Value) -> PermissionRequest {
        PermissionRequest::new("skill_read", args["name"].as_str().unwrap_or("*"))
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult<ToolOutcome> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| Error::InvalidInput("missing name".into()))?;
        let text = self.skills.read(name, args["file"].as_str())?;
        Ok(ToolOutcome::text(text))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn loads_lists_and_reads_skills() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "weekly-report/SKILL.md",
            "---\nname: Weekly report\ndescription: \"How we write the Friday report\"\n---\n# Steps\n1. Pull metrics",
        );
        write(dir.path(), "weekly-report/template.md", "## Wins\n## Risks");
        write(dir.path(), "bare/SKILL.md", "Just text, no frontmatter");
        write(dir.path(), "not-a-skill/README.md", "ignored");

        let skills = Skills::load(dir.path()).unwrap();
        let listing = skills.listing().unwrap();
        assert!(
            listing.contains("- Weekly report (`weekly-report`): How we write the Friday report")
        );
        assert!(listing.contains("(`bare`): No description"));
        assert!(!listing.contains("not-a-skill"));

        let body = skills.read("weekly-report", None).unwrap();
        assert!(body.starts_with("# Steps"), "{body}");
        assert!(body.contains("template.md"));
        assert_eq!(
            skills.read("Weekly Report", Some("template.md")).unwrap(),
            "## Wins\n## Risks"
        );
        assert!(
            skills
                .read("weekly-report", Some("../bare/SKILL.md"))
                .is_err()
        );
        assert!(skills.read("nope", None).is_err());
        assert_eq!(skills.tools().len(), 1);
    }

    #[test]
    fn a_missing_folder_means_no_skills() {
        let skills = Skills::load(Path::new("definitely/not/here")).unwrap();
        assert!(skills.listing().is_none());
        assert!(skills.tools().is_empty());
    }
}
