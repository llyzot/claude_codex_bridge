//! Existing compact/TOML/hybrid config and project identity, without Python.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn project_root(cwd: &Path) -> Result<PathBuf> {
    let mut path = cwd.canonicalize()?;
    loop {
        if path.join(".ccb/ccb.config").is_file() {
            return Ok(path);
        }
        if !path.pop() {
            anyhow::bail!("no .ccb/ccb.config found above current directory");
        }
    }
}
pub fn project_id(root: &Path) -> Result<String> {
    let identity = root.join(".ccb/project.identity.json");
    if identity.is_file() {
        let value: Value = serde_json::from_slice(&std::fs::read(identity)?)?;
        ensure!(
            value["schema_version"] == 1 && value["record_type"] == "ccb_project_identity",
            "invalid project identity schema"
        );
        let id = value["project_id"]
            .as_str()
            .context("project identity missing id")?;
        ensure!(
            id.len() == 64
                && id
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid project id"
        );
        return Ok(id.into());
    }
    let path = root.canonicalize()?.to_string_lossy().into_owned();
    #[cfg(windows)]
    let path = if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(&path).to_owned()
    };
    let path = path.replace('\\', "/");
    #[cfg(windows)]
    let path = path.to_lowercase();
    Ok(format!("{:x}", Sha256::digest(path.as_bytes())))
}
pub fn parse(text: &str) -> Result<Value> {
    if text.trim_start().starts_with('{') {
        return Ok(serde_json::from_str(text)?);
    }
    let lines = text.lines().collect::<Vec<_>>();
    let rich = lines.iter().position(|line| {
        let s = line.split('#').next().unwrap_or("").trim();
        !s.is_empty() && (s.starts_with('[') || s.contains('='))
    });
    let first = lines
        .iter()
        .position(|line| !line.split('#').next().unwrap_or("").trim().is_empty())
        .context("config is empty")?;
    if rich == Some(first) {
        let value: toml::Value = toml::from_str(text)?;
        return Ok(serde_json::to_value(value)?);
    }
    let header = lines[..rich.unwrap_or(lines.len())].join("\n");
    let mut parser = Layout::new(&header)?;
    let layout = parser.horizontal()?;
    ensure!(
        parser.index == parser.tokens.len(),
        "unexpected compact layout token"
    );
    ensure!(
        !parser.names.is_empty(),
        "compact config must define at least one agent"
    );
    let mut document = json!({"version":2,"default_agents":parser.names,"agents":parser.agents,"cmd_enabled":parser.cmd,"layout":layout});
    if let Some(index) = rich {
        let overlay: toml::Value = toml::from_str(&lines[index..].join("\n"))?;
        let overlay = serde_json::to_value(overlay)?;
        for (key, value) in overlay
            .as_object()
            .context("hybrid overlay must be a table")?
        {
            ensure!(
                ["agents", "maintenance", "loop", "config_ui"].contains(&key.as_str()),
                "unsupported hybrid top-level key: {key}"
            );
            if key == "agents" {
                for (name, options) in value.as_object().context("hybrid agents must be a table")? {
                    let target = document["agents"][name]
                        .as_object_mut()
                        .context("hybrid overlay names an agent absent from layout")?;
                    for (field, value) in options
                        .as_object()
                        .context("hybrid agent options must be a table")?
                    {
                        ensure!(
                            !["provider", "workspace_mode"].contains(&field.as_str()),
                            "hybrid cannot override header-owned agent field: {field}"
                        );
                        target.insert(field.clone(), value.clone());
                    }
                }
            } else {
                document[key] = value.clone();
            }
        }
    }
    Ok(document)
}
struct Layout {
    tokens: Vec<String>,
    index: usize,
    agents: serde_json::Map<String, Value>,
    names: Vec<String>,
    cmd: bool,
}
impl Layout {
    fn new(text: &str) -> Result<Self> {
        let mut tokens = Vec::new();
        let mut leaf = String::new();
        for line in text.lines() {
            let line = line
                .split('#')
                .next()
                .unwrap_or("")
                .split("//")
                .next()
                .unwrap_or("");
            let chars = line.chars().collect::<Vec<_>>();
            let mut i = 0;
            while i < chars.len() {
                let ch = chars[i];
                if ch == '(' && !leaf.trim().is_empty() {
                    if let Some(end) = chars[i..].iter().position(|c| *c == ')') {
                        leaf.extend(chars[i..=i + end].iter());
                        i += end + 1;
                        continue;
                    }
                }
                if "(),;".contains(ch) {
                    if !leaf.trim().is_empty() {
                        tokens.push(leaf.trim().into());
                    }
                    leaf.clear();
                    tokens.push(ch.to_string());
                } else {
                    leaf.push(ch);
                }
                i += 1;
            }
            if !leaf.trim().is_empty() {
                tokens.push(leaf.trim().into());
            }
            leaf.clear();
        }
        ensure!(!tokens.is_empty(), "layout is empty");
        Ok(Self {
            tokens,
            index: 0,
            agents: serde_json::Map::new(),
            names: Vec::new(),
            cmd: false,
        })
    }
    fn peek(&self) -> &str {
        self.tokens
            .get(self.index)
            .map(String::as_str)
            .unwrap_or("")
    }
    fn take(&mut self) -> Result<String> {
        let token = self
            .tokens
            .get(self.index)
            .cloned()
            .context("unexpected end of layout")?;
        self.index += 1;
        Ok(token)
    }
    fn horizontal(&mut self) -> Result<String> {
        let mut value = self.vertical()?;
        while self.peek() == ";" {
            self.take()?;
            value = format!("{value}; {}", self.vertical()?);
        }
        Ok(value)
    }
    fn vertical(&mut self) -> Result<String> {
        let mut value = self.primary()?;
        while self.peek() == "," {
            self.take()?;
            value = format!("{value}, {}", self.primary()?);
        }
        Ok(value)
    }
    fn primary(&mut self) -> Result<String> {
        if self.peek() == "(" {
            self.take()?;
            let value = self.horizontal()?;
            ensure!(self.take()? == ")", "unclosed layout group");
            return Ok(format!("({value})"));
        }
        let token = self.take()?;
        ensure!(
            ![";", ",", ")"].contains(&token.as_str()),
            "unexpected layout token"
        );
        let (leaf, percent) = if let Some((leaf, pct)) = token.rsplit_once('@') {
            let pct = pct.trim().parse::<u32>()?;
            ensure!((1..=99).contains(&pct), "layout percent must be 1..99");
            (leaf.trim(), Some(pct))
        } else {
            (token.trim(), None)
        };
        if leaf == "cmd" {
            ensure!(!self.cmd, "compact config cannot define cmd more than once");
            self.cmd = true;
            return Ok(token);
        }
        let (name, provider) = leaf
            .split_once(':')
            .context("compact agent requires name:provider")?;
        let name = name.trim().to_lowercase();
        crate::models::validate_agent(&name)?;
        ensure!(
            !self.agents.contains_key(&name),
            "duplicate compact agent name"
        );
        let mut provider = provider.trim().to_owned();
        let worktree = provider.ends_with("(worktree)");
        if worktree {
            provider = provider.trim_end_matches("(worktree)").trim().into();
        }
        ensure!(
            !provider.is_empty()
                && provider
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
            "invalid compact provider"
        );
        self.names.push(name.clone());
        self.agents.insert(name.clone(),json!({"provider":provider,"target":".","workspace_mode":if worktree{"git-worktree"}else{"inplace"},"restore":"auto","permission":"manual"}));
        let mut render = format!(
            "{name}:{provider}{}",
            if worktree { "(worktree)" } else { "" }
        );
        if let Some(percent) = percent {
            render.push_str(&format!("@{percent}"));
        }
        Ok(render)
    }
}
pub fn compile(root: &Path, document: Value, executables: &Value) -> Result<Value> {
    let id = project_id(root)?;
    let mut agents = serde_json::Map::new();
    for (name, raw) in document["agents"]
        .as_object()
        .context("config agents must be a table")?
    {
        crate::models::validate_agent(name)?;
        let provider = raw["provider"].as_str().context("agent provider missing")?;
        ensure!(
            raw["workspace_mode"].is_null() || raw["workspace_mode"] == "inplace",
            "native workspace mode not yet supported: {}",
            raw["workspace_mode"]
        );
        let target = raw["target"].as_str().unwrap_or(".");
        let workspace = root.join(target);
        ensure!(workspace.is_dir(), "agent target directory does not exist");
        let mut spec = raw.clone();
        spec["workspace_path"] = json!(workspace.canonicalize()?);
        let command = executables[provider].as_array().context(format!(
            "native executable argv missing for provider: {provider}"
        ))?;
        let mut argv = command.clone();
        if let Some(args) = raw["startup_args"].as_array() {
            argv.extend(args.iter().cloned());
        }
        match provider {
            "pi" => {
                argv.extend([json!("--mode"), json!("rpc")]);
                if let Some(model) = raw["model"].as_str() {
                    argv.extend([json!("--model"), json!(model)]);
                }
                if let Some(thinking) = raw["thinking"].as_str() {
                    argv.extend([json!("--thinking"), json!(thinking)]);
                }
                spec["driver_kind"] = json!("pi_rpc");
                spec["profile_family"] = json!("protocol_turn");
            }
            "codex" => {
                argv.extend([json!("app-server"), json!("--stdio")]);
                spec["driver_kind"] = json!("codex_app_server");
                spec["profile_family"] = json!("protocol_turn");
                let mut options = spec["thread_options"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                if let Some(model) = raw["model"].as_str() {
                    options.insert("model".into(), json!(model));
                }
                spec["thread_options"] = json!(options);
                if let Some(thinking) = raw["thinking"].as_str() {
                    spec["turn_options"] = json!({"effort":thinking});
                }
            }
            "claude" => {
                argv.extend([
                    json!("-p"),
                    json!("--verbose"),
                    json!("--input-format"),
                    json!("stream-json"),
                    json!("--output-format"),
                    json!("stream-json"),
                    json!("--replay-user-messages"),
                ]);
                if let Some(model) = raw["model"].as_str() {
                    argv.extend([json!("--model"), json!(model)]);
                }
                spec["driver_kind"] = json!("claude_stream");
                spec["profile_family"] = json!("structured_result");
            }
            _ => anyhow::bail!("native provider adapter is not implemented: {provider}"),
        }
        spec["command"] = json!(argv);
        agents.insert(name.clone(), spec);
    }
    ensure!(!agents.is_empty(), "config has no agents");
    Ok(
        json!({"schema_version":2,"project_id":id,"project_root":root,"agents":agents,"source_config":document}),
    )
}
