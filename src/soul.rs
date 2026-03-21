use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct SoulSeed {
    pub name:             String,
    pub vigor:            u32,
    pub wit:              u32,
    pub grace:            u32,
    pub heart:            u32,
    pub numen:            u32,
    pub specialty:        Option<String>,
    pub personality:      String,
    pub backstory:        String,
    pub magical_affinity: String,
    pub self_declaration: String,
}

pub fn load_all(dir: &str) -> Result<Vec<SoulSeed>, Box<dyn std::error::Error + Send + Sync>> {
    let path = Path::new(dir);
    if !path.exists() {
        return Err(format!("Souls directory '{}' does not exist", dir).into());
    }

    let mut souls = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry    = entry?;
        let file_name = entry.file_name();
        let name      = file_name.to_string_lossy();

        if name.ends_with(".seed.md") {
            let content = fs::read_to_string(entry.path())?;
            let soul = parse(&content).map_err(|e| {
                format!("Failed to parse soul seed '{}': {}", name, e)
            })?;
            souls.push(soul);
        }
    }

    if souls.is_empty() {
        return Err(format!(
            "No *.seed.md files found in '{}'. Create soul seeds first.", dir
        ).into());
    }

    // Consistent ordering so determinism is preserved across runs
    souls.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(souls)
}

pub fn parse(content: &str) -> Result<SoulSeed, Box<dyn std::error::Error + Send + Sync>> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let fm = parse_kv(&frontmatter);

    let get = |key: &str| -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        fm.get(key)
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .ok_or_else(|| format!("missing frontmatter key: {}", key).into())
    };
    let get_u32 = |key: &str| -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        get(key)?.parse::<u32>().map_err(|e| format!("bad value for '{}': {}", key, e).into())
    };

    let vigor = get_u32("vigor")?;
    let wit   = get_u32("wit")?;
    let grace = get_u32("grace")?;
    let heart = get_u32("heart")?;
    let numen = get_u32("numen")?;
    let sum   = vigor + wit + grace + heart + numen;
    if sum != 30 {
        return Err(format!(
            "Attributes must sum to 30, got {} (V:{} W:{} G:{} H:{} N:{})",
            sum, vigor, wit, grace, heart, numen
        ).into());
    }

    let name = get("name")?;
    validate_name_safe(&name)?;

    let sections = parse_sections(&body);
    Ok(SoulSeed {
        name,
        vigor, wit, grace, heart, numen,
        specialty:    fm.get("specialty").map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string()).filter(|s| !s.is_empty()),
        personality:  sections.get("Personality").cloned().unwrap_or_default(),
        backstory:    sections.get("Backstory").cloned().unwrap_or_default(),
        magical_affinity: sections.get("Magical Affinity").cloned().unwrap_or_default(),
        self_declaration: sections.get("Self-Declaration").cloned().unwrap_or_default(),
    })
}

/// Reject names that could escape the souls directory when used as path components.
/// Names must be non-empty and must not contain path separators (`/`, `\`)
/// or any occurrence of the parent-directory token (`..`).
fn validate_name_safe(name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if name.is_empty() {
        return Err("Soul name must not be empty".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err(format!(
            "Soul name '{}' contains a path separator — names must not include '/' or '\\'",
            name
        ).into());
    }
    if name.contains("..") {
        return Err(format!(
            "Soul name '{}' contains '..' which is not allowed in soul names",
            name
        ).into());
    }
    Ok(())
}

fn split_frontmatter(
    content: &str,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let content = content.trim();
    if !content.starts_with("---") {
        return Err("Soul seed must begin with '---' frontmatter delimiter".into());
    }
    let rest = &content[3..];
    // Look for closing --- on its own line
    let end = rest
        .find("\n---")
        .ok_or("Frontmatter not closed — missing second '---'")?;
    let frontmatter = rest[..end].trim().to_string();
    let body        = rest[end + 4..].trim().to_string();
    Ok((frontmatter, body))
}

fn parse_kv(frontmatter: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in frontmatter.lines() {
        if let Some((key, value)) = line.split_once(':') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_all_returns_souls_with_valid_attributes() {
        let souls = load_all("souls").expect("souls/ directory should exist and be parseable");
        assert!(!souls.is_empty(), "must have at least one soul seed");
        for soul in &souls {
            let sum = soul.vigor + soul.wit + soul.grace + soul.heart + soul.numen;
            assert_eq!(sum, 30, "attributes for {} must sum to 30, got {}", soul.name, sum);
            assert!(!soul.name.is_empty(), "soul name must not be empty");
            assert!(!soul.personality.is_empty(), "personality must not be empty for {}", soul.name);
        }
    }

    #[test]
    fn parse_minimal_seed() {
        let content = "---\nname: Test\nvigor: 6\nwit: 6\ngrace: 6\nheart: 6\nnumen: 6\n---\n## Personality\nCurious.\n## Backstory\nUnknown.\n## Magical Affinity\nNone.\n## Self-Declaration\nI am Test.";
        let soul = parse(content).expect("minimal seed should parse");
        assert_eq!(soul.name, "Test");
        assert_eq!(soul.vigor + soul.wit + soul.grace + soul.heart + soul.numen, 30);
    }

    #[test]
    fn parse_rejects_wrong_attribute_sum() {
        let content = "---\nname: Bad\nvigor: 1\nwit: 1\ngrace: 1\nheart: 1\nnumen: 1\n---\n";
        assert!(parse(content).is_err(), "should reject attribute sum != 30");
    }

    #[test]
    fn parse_rejects_name_with_path_separator() {
        let traversal_seed = "---\nname: ../evil\nvigor: 6\nwit: 6\ngrace: 6\nheart: 6\nnumen: 6\n---\n## Personality\nEvil.\n## Backstory\nN/A.\n## Magical Affinity\nNone.\n## Self-Declaration\nI escape.";
        assert!(parse(traversal_seed).is_err(), "should reject name with path traversal");
    }

    #[test]
    fn parse_rejects_name_with_forward_slash() {
        let slash_seed = "---\nname: a/b\nvigor: 6\nwit: 6\ngrace: 6\nheart: 6\nnumen: 6\n---\n## Personality\nX.\n## Backstory\nX.\n## Magical Affinity\nX.\n## Self-Declaration\nX.";
        assert!(parse(slash_seed).is_err(), "should reject name containing '/'");
    }

    #[test]
    fn validate_name_safe_accepts_normal_names() {
        assert!(validate_name_safe("Elara").is_ok());
        assert!(validate_name_safe("Rowan the Swift").is_ok());
        assert!(validate_name_safe("Agent-7").is_ok());
    }

    #[test]
    fn validate_name_safe_rejects_traversal_sequences() {
        assert!(validate_name_safe("..").is_err());
        assert!(validate_name_safe("../etc").is_err());
        assert!(validate_name_safe("foo..bar").is_err());
        assert!(validate_name_safe("a/b").is_err());
        assert!(validate_name_safe("a\\b").is_err());
        assert!(validate_name_safe("").is_err());
    }
}

fn parse_sections(body: &str) -> HashMap<String, String> {
    let mut sections: HashMap<String, String> = HashMap::new();
    let mut current: Option<String>           = None;
    let mut buf: Vec<&str>                    = Vec::new();

    for line in body.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            if let Some(name) = current.take() {
                sections.insert(name, buf.join("\n").trim().to_string());
                buf.clear();
            }
            current = Some(heading.trim().to_string());
        } else if current.is_some() && !line.starts_with("# ") {
            buf.push(line);
        }
    }
    if let Some(name) = current {
        sections.insert(name, buf.join("\n").trim().to_string());
    }
    sections
}
