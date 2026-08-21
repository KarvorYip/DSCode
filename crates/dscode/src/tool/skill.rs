//! Local skill discovery and secure `skill://` resolution (tools.zh.md §8).

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillSource {
    ProjectDscode,
    UserDscode,
    ProjectClaude,
    UserClaude,
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub globs: Option<serde_yaml::Value>,
    pub always_apply: bool,
    pub hide: bool,
    pub disable_model_invocation: bool,
    pub extra: BTreeMap<String, serde_yaml::Value>,
    pub root: PathBuf,
    pub source: SkillSource,
}

#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    skills: BTreeMap<String, Skill>,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    globs: Option<serde_yaml::Value>,
    #[serde(default, rename = "alwaysApply")]
    always_apply: bool,
    #[serde(default)]
    hide: bool,
    #[serde(default, rename = "disableModelInvocation")]
    disable_model_invocation: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

impl SkillCatalog {
    pub fn discover(project_root: &Path) -> Self {
        Self::discover_in(project_root, dirs::home_dir().as_deref())
    }

    pub fn discover_in(project_root: &Path, home: Option<&Path>) -> Self {
        let mut catalog = Self::default();
        let mut layers = vec![(
            project_root.join(".dscode").join("skills"),
            SkillSource::ProjectDscode,
        )];
        if let Some(home) = home {
            layers.push((home.join(".dscode").join("skills"), SkillSource::UserDscode));
        }
        layers.push((
            project_root.join(".claude").join("skills"),
            SkillSource::ProjectClaude,
        ));
        if let Some(home) = home {
            layers.push((home.join(".claude").join("skills"), SkillSource::UserClaude));
        }

        for (dir, source) in layers {
            catalog.read_layer(&dir, source);
        }
        catalog
    }

    fn read_layer(&mut self, dir: &Path, source: SkillSource) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_dir())
                    .map(|_| entry.path().join("SKILL.md"))
            })
            .filter(|path| path.is_file())
            .collect();
        files.sort();

        for file in files {
            let parsed = std::fs::read_to_string(&file)
                .map_err(|error| format!("读取失败：{error}"))
                .and_then(|text| parse_frontmatter(&text, file.parent().unwrap_or(dir), source));
            match parsed {
                Ok(skill) if self.skills.contains_key(&skill.name) => {
                    eprintln!(
                        "[skill] 同名技能已被高优先级来源遮蔽，跳过 {}：{}",
                        skill.name,
                        file.display()
                    );
                }
                Ok(skill) => {
                    self.skills.insert(skill.name.clone(), skill);
                }
                Err(error) => {
                    eprintln!("[skill] 跳过非法定义 {}：{error}", file.display());
                }
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn prompt_suffix(&self) -> String {
        let visible: Vec<&Skill> = self.skills.values().filter(|skill| !skill.hide).collect();
        if visible.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\n本地 skills（需要时用 read 读取 skill://<name>；正文不会预先注入）：",
        );
        for skill in visible {
            out.push_str(&format!("\n- {}：{}", skill.name, skill.description));
        }
        out
    }

    pub fn resolve_uri(&self, uri: &str) -> Result<PathBuf, String> {
        let rest = uri
            .strip_prefix("skill://")
            .ok_or_else(|| format!("不是 skill:// URI：{uri}"))?;
        if rest.is_empty() || rest.starts_with(['/', '\\']) || Path::new(rest).is_absolute() {
            return Err(format!(
                "skill:// 路径必须以技能名开头且不能是绝对路径：{uri}"
            ));
        }
        let mut parts = rest.split(['/', '\\']);
        let name = parts.next().unwrap_or_default();
        if name.is_empty() || name == "." || name == ".." {
            return Err(format!("skill:// 技能名非法：{uri}"));
        }
        let relative: Vec<&str> = parts.collect();
        if relative.iter().any(|part| *part == "..") {
            return Err(format!("skill:// 路径不允许 .. 逃逸：{uri}"));
        }
        let skill = self
            .skills
            .get(name)
            .ok_or_else(|| format!("未知 skill：{name}"))?;
        let target = if relative.is_empty() {
            skill.root.join("SKILL.md")
        } else {
            relative
                .iter()
                .fold(skill.root.clone(), |path, part| path.join(part))
        };
        let root = std::fs::canonicalize(&skill.root)
            .map_err(|error| format!("无法解析 skill 根目录 {}：{error}", skill.root.display()))?;
        let canonical = std::fs::canonicalize(&target)
            .map_err(|error| format!("skill 资源不存在 {}：{error}", target.display()))?;
        if !canonical.starts_with(&root) {
            return Err(format!("skill:// 路径逃出技能目录：{uri}"));
        }
        Ok(canonical)
    }
}

pub fn parse_frontmatter(text: &str, root: &Path, source: SkillSource) -> Result<Skill, String> {
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return Err("缺少 frontmatter 起始 ---".into());
    }
    let mut yaml = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line == "---" {
            closed = true;
            break;
        }
        yaml.push(line);
    }
    if !closed {
        return Err("缺少 frontmatter 结束 ---".into());
    }
    let frontmatter: Frontmatter = serde_yaml::from_str(&yaml.join("\n"))
        .map_err(|error| format!("frontmatter 解析失败：{error}"))?;
    let name = frontmatter
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .ok_or("缺少必填字段 name")?;
    if name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(format!("skill name 必须是单个安全路径段：{name}"));
    }
    Ok(Skill {
        name,
        description: frontmatter.description.trim().to_string(),
        globs: frontmatter.globs,
        always_apply: frontmatter.always_apply,
        hide: frontmatter.hide,
        disable_model_invocation: frontmatter.disable_model_invocation,
        extra: frontmatter.extra,
        root: root.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, source: &str, dir_name: &str, body: &str) {
        let dir = root.join(source).join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn 同名技能按原生项目到claude用户的优先级首个胜出() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let doc =
            |description: &str| format!("---\nname: same\ndescription: {description}\n---\n正文");
        write_skill(project.path(), ".dscode/skills", "a", &doc("项目原生"));
        write_skill(home.path(), ".dscode/skills", "b", &doc("用户原生"));
        write_skill(project.path(), ".claude/skills", "c", &doc("项目 Claude"));
        write_skill(home.path(), ".claude/skills", "d", &doc("用户 Claude"));

        let catalog = SkillCatalog::discover_in(project.path(), Some(home.path()));
        let skill = catalog.get("same").unwrap();
        assert_eq!(skill.description, "项目原生");
        assert_eq!(skill.source, SkillSource::ProjectDscode);
    }

    #[test]
    fn 非法frontmatter确定报错且发现时跳过() {
        let project = tempfile::tempdir().unwrap();
        write_skill(
            project.path(),
            ".claude/skills",
            "bad",
            "---\nname: [\n---\n正文",
        );
        assert!(parse_frontmatter(
            "---\nname: [\n---\n正文",
            project.path(),
            SkillSource::ProjectClaude
        )
        .unwrap_err()
        .contains("frontmatter 解析失败"));
        assert!(SkillCatalog::discover_in(project.path(), None).is_empty());
    }

    #[test]
    fn skill路由拒绝父目录与绝对路径并允许目录内资源() {
        let project = tempfile::tempdir().unwrap();
        write_skill(
            project.path(),
            ".claude/skills",
            "safe",
            "---\nname: safe\ndescription: 安全\n---\n正文",
        );
        let resource = project.path().join(".claude/skills/safe").join("ref.txt");
        std::fs::write(&resource, "ok").unwrap();
        let catalog = SkillCatalog::discover_in(project.path(), None);
        assert_eq!(
            catalog.resolve_uri("skill://safe/ref.txt").unwrap(),
            std::fs::canonicalize(resource).unwrap()
        );
        assert!(catalog
            .resolve_uri("skill://safe/../outside")
            .unwrap_err()
            .contains("不允许 .."));
        assert!(catalog.resolve_uri("skill:///tmp/x").is_err());
    }

    #[test]
    fn 隐藏技能不进提示且未知字段保留() {
        let parsed = parse_frontmatter(
            "---\nname: hidden\ndescription: 私有\nhide: true\ncustom: 7\n---\n正文",
            Path::new("."),
            SkillSource::ProjectDscode,
        )
        .unwrap();
        assert_eq!(
            parsed
                .extra
                .get("custom")
                .and_then(serde_yaml::Value::as_i64),
            Some(7)
        );
        let catalog = SkillCatalog {
            skills: BTreeMap::from([(parsed.name.clone(), parsed)]),
        };
        assert!(catalog.prompt_suffix().is_empty());
    }
}
