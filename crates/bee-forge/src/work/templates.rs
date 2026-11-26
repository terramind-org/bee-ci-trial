//! Issue templates (Markdown with front matter) and issue forms (YAML), parsed from the
//! default ref's `.beehive/ISSUE_TEMPLATE/` (or `.github/ISSUE_TEMPLATE/`).
//!
//! The file content comes in through [`TemplateSource`], which the server backs with the VCS
//! store; nothing here reads a repo.

use async_trait::async_trait;
use bee_proto::v1::{
    IssueContactLink, IssueFormField, IssueFormFieldKind, IssueFormFieldOption, IssueTemplate,
    IssueTemplateConfig, KeyValue, Repo,
};
use bee_workflow::yaml::{self, Node, NodeKind, Scalar};

use super::*;

/// Where templates live, in order of preference.
pub const TEMPLATE_DIRS: [&str; 2] = [".beehive/ISSUE_TEMPLATE", ".github/ISSUE_TEMPLATE"];

/// One file of a template directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemplateFile {
    /// Path from the repo root, e.g. `.beehive/ISSUE_TEMPLATE/bug_report.yml`.
    pub path: String,
    /// Raw content.
    pub content: Vec<u8>,
}

/// The files of a template directory at one revision.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemplateFiles {
    /// The changeset the files were read from.
    pub revision: String,
    /// Files directly inside the directory.
    pub files: Vec<TemplateFile>,
}

/// Reads template directories from the default ref.
#[async_trait]
pub trait TemplateSource: Send + Sync {
    /// The files directly inside `dir` on `repo`'s default ref, or `None` when it does not
    /// exist.
    async fn read_dir(&self, repo: &Repo, dir: &str) -> Result<Option<TemplateFiles>>;
}

/// No repo content is attached: a repo has no templates.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTemplates;

#[async_trait]
impl TemplateSource for NoTemplates {
    async fn read_dir(&self, _: &Repo, _: &str) -> Result<Option<TemplateFiles>> {
        Ok(None)
    }
}

fn scalar_text(n: &Node) -> Option<String> {
    match n.scalar()? {
        Scalar::Null => Some(String::new()),
        _ => n.as_str().map(str::to_string),
    }
}

fn bool_of(n: &Node) -> Option<bool> {
    match n.scalar()? {
        Scalar::Bool(b) => Some(b),
        Scalar::Str(s) => match s.to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A scalar or a sequence of scalars, as a list; a comma-separated scalar splits.
fn string_list(n: Option<&Node>) -> Vec<String> {
    let Some(n) = n else { return Vec::new() };
    match &n.kind {
        NodeKind::Seq(items) => items
            .iter()
            .filter_map(scalar_text)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        NodeKind::Scalar(..) => scalar_text(n)
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        NodeKind::Map(_) => Vec::new(),
    }
}

fn template_from(file_name: &str, path: &str, doc: &Node) -> IssueTemplate {
    IssueTemplate {
        file_name: file_name.into(),
        path: path.into(),
        name: doc.get("name").and_then(scalar_text).unwrap_or_default(),
        about: doc
            .get("about")
            .or_else(|| doc.get("description"))
            .and_then(scalar_text)
            .unwrap_or_default(),
        title: doc.get("title").and_then(scalar_text).unwrap_or_default(),
        labels: string_list(doc.get("labels")),
        assignees: string_list(doc.get("assignees")),
        issue_type: doc.get("type").and_then(scalar_text).unwrap_or_default(),
        projects: string_list(doc.get("projects")),
        ..Default::default()
    }
}

fn bad(file_name: &str, path: &str, msg: impl Into<String>) -> IssueTemplate {
    IssueTemplate {
        file_name: file_name.into(),
        path: path.into(),
        name: file_name.into(),
        parse_error: msg.into(),
        ..Default::default()
    }
}

fn form_kind(s: &str) -> Option<IssueFormFieldKind> {
    Some(match s {
        "markdown" => IssueFormFieldKind::Markdown,
        "input" => IssueFormFieldKind::Input,
        "textarea" => IssueFormFieldKind::Textarea,
        "dropdown" => IssueFormFieldKind::Dropdown,
        "checkboxes" => IssueFormFieldKind::Checkboxes,
        "upload" => IssueFormFieldKind::Upload,
        _ => return None,
    })
}

fn parse_form_field(index: usize, el: &Node) -> std::result::Result<IssueFormField, String> {
    let ty = el
        .get("type")
        .and_then(scalar_text)
        .ok_or_else(|| format!("body[{index}] has no `type`"))?;
    let kind = form_kind(&ty).ok_or_else(|| format!("body[{index}] has unknown type {ty:?}"))?;
    let attrs = el.get("attributes");
    let attr = |k: &str| {
        attrs
            .and_then(|a| a.get(k))
            .and_then(scalar_text)
            .unwrap_or_default()
    };
    let id = el.get("id").and_then(scalar_text).unwrap_or_default();
    if !id.is_empty()
        && !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!(
            "body[{index}] id {id:?} may use letters, digits, '-' and '_' only"
        ));
    }
    let required = el
        .get("validations")
        .and_then(|v| v.get("required"))
        .and_then(bool_of)
        .unwrap_or(false);
    let mut f = IssueFormField {
        id: if id.is_empty() {
            format!("field-{index}")
        } else {
            id
        },
        kind: kind as i32,
        label: attr("label"),
        description: attr("description"),
        placeholder: attr("placeholder"),
        default_value: attr("value"),
        required,
        render: attr("render"),
        ..Default::default()
    };
    match kind {
        IssueFormFieldKind::Markdown => {
            if attr("value").is_empty() {
                return Err(format!("body[{index}] markdown needs `attributes.value`"));
            }
            f.label.clear();
        }
        IssueFormFieldKind::Dropdown => {
            let opts = string_list(attrs.and_then(|a| a.get("options")));
            if opts.is_empty() {
                return Err(format!("body[{index}] dropdown needs `attributes.options`"));
            }
            f.options = opts
                .into_iter()
                .map(|label| IssueFormFieldOption {
                    label,
                    required: false,
                })
                .collect();
            f.multiple = attrs
                .and_then(|a| a.get("multiple"))
                .and_then(bool_of)
                .unwrap_or(false);
        }
        IssueFormFieldKind::Checkboxes => {
            let items = attrs
                .and_then(|a| a.get("options"))
                .and_then(|o| o.as_seq())
                .ok_or_else(|| format!("body[{index}] checkboxes needs `attributes.options`"))?;
            for it in items {
                let label = it
                    .get("label")
                    .and_then(scalar_text)
                    .ok_or_else(|| format!("body[{index}] checkbox option has no `label`"))?;
                f.options.push(IssueFormFieldOption {
                    label,
                    required: it.get("required").and_then(bool_of).unwrap_or(false),
                });
            }
            if f.options.is_empty() {
                return Err(format!("body[{index}] checkboxes needs options"));
            }
        }
        _ => {}
    }
    if kind != IssueFormFieldKind::Markdown && f.label.trim().is_empty() {
        return Err(format!("body[{index}] needs `attributes.label`"));
    }
    Ok(f)
}

/// Parses one template file; an invalid file yields a template whose `parse_error` is set.
pub fn parse_template(file_name: &str, path: &str, content: &[u8]) -> Option<IssueTemplate> {
    let lower = file_name.to_ascii_lowercase();
    let text = match std::str::from_utf8(content) {
        Ok(t) => t.trim_start_matches('\u{feff}'),
        Err(_) => return Some(bad(file_name, path, "the file is not UTF-8")),
    };
    if lower.ends_with(".yml") || lower.ends_with(".yaml") {
        if lower == "config.yml" || lower == "config.yaml" {
            return None;
        }
        let doc = match yaml::parse(text) {
            Ok(d) => d,
            Err(e) => return Some(bad(file_name, path, format!("invalid YAML at {e}"))),
        };
        if doc.as_map().is_none() {
            return Some(bad(file_name, path, "an issue form is a YAML mapping"));
        }
        let mut t = template_from(file_name, path, &doc);
        t.is_form = true;
        if t.name.trim().is_empty() {
            return Some(bad(file_name, path, "an issue form needs a `name`"));
        }
        if t.about.trim().is_empty() {
            return Some(bad(file_name, path, "an issue form needs a `description`"));
        }
        let Some(body) = doc.get("body").and_then(Node::as_seq) else {
            return Some(bad(file_name, path, "an issue form needs a `body` list"));
        };
        let mut ids = std::collections::BTreeSet::new();
        for (i, el) in body.iter().enumerate() {
            match parse_form_field(i, el) {
                Ok(f) => {
                    if !ids.insert(f.id.clone()) {
                        return Some(bad(file_name, path, format!("id {:?} is used twice", f.id)));
                    }
                    t.form_fields.push(f);
                }
                Err(e) => return Some(bad(file_name, path, e)),
            }
        }
        if t.form_fields
            .iter()
            .all(|f| f.kind == IssueFormFieldKind::Markdown as i32)
        {
            return Some(bad(
                file_name,
                path,
                "an issue form needs at least one input",
            ));
        }
        return Some(t);
    }
    if lower.ends_with(".md") {
        let (front, body) = split_front_matter(text);
        let mut t = match front {
            Some(f) => match yaml::parse(f) {
                Ok(doc) if doc.as_map().is_some() => template_from(file_name, path, &doc),
                Ok(_) => return Some(bad(file_name, path, "front matter is not a mapping")),
                Err(e) => {
                    return Some(bad(file_name, path, format!("invalid front matter at {e}")));
                }
            },
            None => IssueTemplate {
                file_name: file_name.into(),
                path: path.into(),
                ..Default::default()
            },
        };
        if t.name.trim().is_empty() {
            t.name = file_name.trim_end_matches(".md").replace(['_', '-'], " ");
        }
        t.body = body.to_string();
        return Some(t);
    }
    None
}

fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let front = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(front), body);
        }
        offset += line.len();
    }
    (None, text)
}

/// Parses `config.yml`.
pub fn parse_config(content: &[u8], revision: &str) -> IssueTemplateConfig {
    let mut cfg = IssueTemplateConfig {
        blank_issues_enabled: true,
        contact_links: Vec::new(),
        revision: revision.to_string(),
    };
    let Ok(text) = std::str::from_utf8(content) else {
        return cfg;
    };
    let Ok(doc) = yaml::parse(text) else {
        return cfg;
    };
    if let Some(b) = doc.get("blank_issues_enabled").and_then(bool_of) {
        cfg.blank_issues_enabled = b;
    }
    if let Some(links) = doc.get("contact_links").and_then(Node::as_seq) {
        for l in links {
            let name = l.get("name").and_then(scalar_text).unwrap_or_default();
            let url = l.get("url").and_then(scalar_text).unwrap_or_default();
            if name.is_empty() || url.is_empty() {
                continue;
            }
            cfg.contact_links.push(IssueContactLink {
                name,
                url,
                about: l.get("about").and_then(scalar_text).unwrap_or_default(),
            });
        }
    }
    cfg
}

/// Renders a form's answers into the issue body (`### Label` sections). Required inputs and
/// ticks are checked; checkbox answers list the ticked labels, one per line.
pub fn render_form_body(fields: &[IssueFormField], responses: &[KeyValue]) -> Result<String> {
    let answer = |id: &str| {
        responses
            .iter()
            .find(|r| r.key == id)
            .map(|r| r.value.trim_end().to_string())
            .unwrap_or_default()
    };
    for r in responses {
        if !fields
            .iter()
            .any(|f| f.id == r.key && f.kind != IssueFormFieldKind::Markdown as i32)
        {
            return Err(ForgeError::invalid(
                "unknown_form_field",
                format!("the form has no field {:?}", r.key),
            ));
        }
    }
    let mut out = String::new();
    for f in fields {
        let kind = IssueFormFieldKind::try_from(f.kind).unwrap_or(IssueFormFieldKind::Unspecified);
        if kind == IssueFormFieldKind::Markdown || kind == IssueFormFieldKind::Unspecified {
            continue;
        }
        let a = answer(&f.id);
        let missing = |what: &str| {
            ForgeError::invalid("form_field_required", format!("{:?} {what}", f.label))
        };
        out.push_str(&format!("### {}\n\n", f.label));
        match kind {
            IssueFormFieldKind::Checkboxes => {
                let ticked: Vec<&str> =
                    a.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                for t in &ticked {
                    if !f.options.iter().any(|o| o.label == *t) {
                        return Err(ForgeError::invalid(
                            "unknown_option",
                            format!("{:?} has no option {t:?}", f.label),
                        ));
                    }
                }
                for o in &f.options {
                    let on = ticked.contains(&o.label.as_str());
                    if o.required && !on {
                        return Err(missing(&format!("needs {:?} ticked", o.label)));
                    }
                    out.push_str(&format!("- [{}] {}\n", if on { "x" } else { " " }, o.label));
                }
                out.push('\n');
            }
            IssueFormFieldKind::Dropdown => {
                let picked: Vec<&str> = if f.multiple {
                    a.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect()
                } else if a.is_empty() {
                    Vec::new()
                } else {
                    vec![a.as_str()]
                };
                if !f.multiple && a.contains(',') && !f.options.iter().any(|o| o.label == a) {
                    return Err(missing("allows one choice"));
                }
                for p in &picked {
                    if !f.options.iter().any(|o| o.label == *p) {
                        return Err(ForgeError::invalid(
                            "unknown_option",
                            format!("{:?} has no option {p:?}", f.label),
                        ));
                    }
                }
                if f.required && picked.is_empty() {
                    return Err(missing("is required"));
                }
                out.push_str(&if picked.is_empty() {
                    "_No response_\n\n".to_string()
                } else {
                    format!("{}\n\n", picked.join(", "))
                });
            }
            _ => {
                if f.required && a.trim().is_empty() {
                    return Err(missing("is required"));
                }
                if a.trim().is_empty() {
                    out.push_str("_No response_\n\n");
                } else if kind == IssueFormFieldKind::Textarea && !f.render.is_empty() {
                    out.push_str(&format!("```{}\n{a}\n```\n\n", f.render));
                } else {
                    out.push_str(&format!("{a}\n\n"));
                }
            }
        }
    }
    Ok(out.trim_end().to_string() + "\n")
}

impl Work {
    async fn template_dir(&self, repo: &Repo) -> Result<Option<(String, TemplateFiles)>> {
        let src = self.hooks().templates;
        for d in TEMPLATE_DIRS {
            if let Some(files) = src.read_dir(repo, d).await? {
                return Ok(Some((d.to_string(), files)));
            }
        }
        Ok(None)
    }

    /// Templates and forms of the default ref, by file name, and the revision they came from.
    pub async fn list_templates(&self, repo: &Repo) -> Result<(Vec<IssueTemplate>, String)> {
        let Some((dir, files)) = self.template_dir(repo).await? else {
            return Ok((Vec::new(), String::new()));
        };
        let mut out: Vec<IssueTemplate> = files
            .files
            .iter()
            .filter_map(|f| {
                let name = f.path.rsplit('/').next().unwrap_or(&f.path);
                let path = if f.path.contains('/') {
                    f.path.clone()
                } else {
                    format!("{dir}/{name}")
                };
                parse_template(name, &path, &f.content)
            })
            .collect();
        out.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok((out, files.revision))
    }

    /// One usable template by file name; an unparseable one is `INVALID_ARGUMENT`.
    pub async fn find_template(&self, repo: &Repo, file_name: &str) -> Result<IssueTemplate> {
        let (all, _) = self.list_templates(repo).await?;
        let t = all
            .into_iter()
            .find(|t| t.file_name == file_name)
            .ok_or_else(|| ForgeError::not_found(format!("issue template {file_name:?}")))?;
        if !t.parse_error.is_empty() {
            return Err(ForgeError::invalid(
                "invalid_template",
                format!("template {file_name:?} is unusable: {}", t.parse_error),
            ));
        }
        Ok(t)
    }

    /// The chooser configuration (`config.yml`).
    pub async fn template_config(&self, repo: &Repo) -> Result<IssueTemplateConfig> {
        let Some((_, files)) = self.template_dir(repo).await? else {
            return Ok(IssueTemplateConfig {
                blank_issues_enabled: true,
                ..Default::default()
            });
        };
        let cfg = files.files.iter().find(|f| {
            let n = f
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&f.path)
                .to_ascii_lowercase();
            n == "config.yml" || n == "config.yaml"
        });
        Ok(match cfg {
            Some(f) => parse_config(&f.content, &files.revision),
            None => IssueTemplateConfig {
                blank_issues_enabled: true,
                revision: files.revision,
                ..Default::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORM: &str = "name: Bug report\ndescription: File a bug\ntitle: \"[Bug]: \"\nlabels: [bug, triage]\nassignees: octo\ntype: Bug\nbody:\n  - type: markdown\n    attributes:\n      value: Thanks!\n  - type: input\n    id: version\n    attributes:\n      label: Version\n    validations:\n      required: true\n  - type: textarea\n    id: logs\n    attributes:\n      label: Logs\n      render: shell\n  - type: dropdown\n    id: sev\n    attributes:\n      label: Severity\n      options: [low, high]\n  - type: checkboxes\n    id: terms\n    attributes:\n      label: Terms\n      options:\n        - label: I agree\n          required: true\n        - label: Ping me\n";

    #[test]
    fn forms_parse_and_render() {
        let t = parse_template(
            "bug.yml",
            ".beehive/ISSUE_TEMPLATE/bug.yml",
            FORM.as_bytes(),
        )
        .unwrap();
        assert!(t.parse_error.is_empty(), "{}", t.parse_error);
        assert!(t.is_form);
        assert_eq!(t.labels, vec!["bug", "triage"]);
        assert_eq!(t.assignees, vec!["octo"]);
        assert_eq!(t.issue_type, "Bug");
        assert_eq!(t.form_fields.len(), 5);
        let kv = |k: &str, v: &str| KeyValue {
            key: k.into(),
            value: v.into(),
        };
        let body = render_form_body(
            &t.form_fields,
            &[
                kv("version", "1.2"),
                kv("logs", "boom"),
                kv("sev", "high"),
                kv("terms", "I agree"),
            ],
        )
        .unwrap();
        assert!(body.contains("### Version\n\n1.2"));
        assert!(body.contains("```shell\nboom\n```"));
        assert!(body.contains("- [x] I agree\n- [ ] Ping me"));
        let missing = render_form_body(&t.form_fields, &[kv("terms", "I agree")]).unwrap_err();
        assert_eq!(missing.subcode, "form_field_required");
        let unticked = render_form_body(&t.form_fields, &[kv("version", "1")]).unwrap_err();
        assert_eq!(unticked.subcode, "form_field_required");
    }

    #[test]
    fn markdown_templates_and_errors() {
        let md =
            "---\nname: Feature\nabout: Ask for a thing\nlabels: enhancement\n---\n## Describe\n";
        let t = parse_template("feature.md", "p/feature.md", md.as_bytes()).unwrap();
        assert_eq!(t.name, "Feature");
        assert_eq!(t.labels, vec!["enhancement"]);
        assert_eq!(t.body, "## Describe\n");
        let plain = parse_template("my_idea.md", "p/my_idea.md", b"hello").unwrap();
        assert_eq!(plain.name, "my idea");
        let broken =
            parse_template("x.yml", "p/x.yml", b"name: X\ndescription: d\nbody: []\n").unwrap();
        assert!(!broken.parse_error.is_empty());
        let dup = "name: X\ndescription: d\nbody:\n  - type: input\n    id: a\n    attributes: {label: A}\n  - type: input\n    id: a\n    attributes: {label: B}\n";
        assert!(
            parse_template("d.yml", "p/d.yml", dup.as_bytes())
                .unwrap()
                .parse_error
                .contains("twice")
        );
        assert!(parse_template("config.yml", "p/config.yml", b"x: 1").is_none());
        assert!(parse_template("README.txt", "p/README.txt", b"x").is_none());
    }

    #[test]
    fn config_reads_links() {
        let c = parse_config(
            b"blank_issues_enabled: false\ncontact_links:\n  - name: Chat\n    url: https://example.com/chat\n    about: Ask there\n  - name: broken\n",
            "rev1",
        );
        assert!(!c.blank_issues_enabled);
        assert_eq!(c.contact_links.len(), 1);
        assert_eq!(c.revision, "rev1");
    }
}
