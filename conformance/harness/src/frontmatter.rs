#![forbid(unsafe_code)]

//! Test262 frontmatter parsing.
//!
//! Test262 files embed YAML between `/*---` and `---*/`. This module parses
//! the subset needed by the harness: `description`, `esid`, `features`,
//! `flags`, `includes`, and `negative` (phase/type).

/// Maximum frontmatter block size in bytes.
///
/// Guards against pathological files that never close the comment.
const MAX_FRONTMATTER_LEN: usize = 16_384;

/// A parsed `negative` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negative {
    /// Phase where the error is expected: `parse`, `resolution`, or `runtime`.
    pub phase: String,
    /// Expected error constructor name, e.g. `SyntaxError`.
    pub type_name: String,
}

/// Parsed Test262 frontmatter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frontmatter {
    /// Human-readable description.
    pub description: Option<String>,
    /// ES spec section id.
    pub esid: Option<String>,
    /// Feature list (e.g. `BigInt`, `async-iteration`).
    pub features: Vec<String>,
    /// Flags (e.g. `async`, `module`, `noStrict`, `onlyStrict`, `raw`).
    pub flags: Vec<String>,
    /// Harness files to prepend (e.g. `assert.js`).
    pub includes: Vec<String>,
    /// Negative expectation, if any.
    pub negative: Option<Negative>,
    /// `info` field (free-form).
    pub info: Option<String>,
}

impl Frontmatter {
    /// Returns true if the test carries the given flag.
    #[must_use]
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    /// Returns true if the test requires the given feature.
    #[must_use]
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// Returns true if the test is expected to fail during the given phase.
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.negative.is_some()
    }
}

/// Extracts the raw YAML between `/*---` and `---*/`, if present.
///
/// Returns `None` when the delimiters are absent.
#[must_use]
pub fn extract_raw_frontmatter(source: &str) -> Option<String> {
    let start = source.find("/*---")?;
    let after_start = &source[start + "/*---".len()..];
    let end = after_start.find("---*/")?;
    let raw = &after_start[..end];
    if raw.len() > MAX_FRONTMATTER_LEN {
        return None;
    }
    Some(raw.to_string())
}

/// Parses frontmatter from a full test source string.
///
/// Returns `Frontmatter::default()` when no frontmatter block exists.
#[must_use]
pub fn parse_frontmatter(source: &str) -> Frontmatter {
    let Some(raw) = extract_raw_frontmatter(source) else {
        return Frontmatter::default();
    };
    parse_yaml_block(&raw)
}

/// Removes the frontmatter comment block from `source`, returning the
/// remainder. If no block is present, returns `source` unchanged.
#[must_use]
pub fn strip_frontmatter<'a>(source: &'a str) -> &'a str {
    if let Some(start) = source.find("/*---") {
        if let Some(end_offset) = source[start..].find("---*/") {
            let end = start + end_offset + "---*/".len();
            return &source[end..];
        }
    }
    source
}

/// Parses a YAML-like frontmatter block into [`Frontmatter`].
///
/// Handles the Test262 subset: scalar keys, inline arrays `[a, b]`, block
/// arrays (`- item`), and the two-level `negative` map.
fn parse_yaml_block(raw: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    let lines: Vec<&str> = raw.lines().collect();
    let mut idx = 0usize;

    while idx < lines.len() {
        let line = lines[idx];
        let trimmed = line.trim();

        // Skip empty lines and comments.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }

        // Detect a top-level key: no leading indent, contains ':'.
        // Indented lines belong to a block value and are consumed by the
        // handler for the preceding key.
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        if is_indented {
            // Should have been consumed as part of previous key's block.
            idx += 1;
            continue;
        }

        if let Some(colon) = trimmed.find(':') {
            let key = trimmed[..colon].trim();
            let value_part = trimmed[colon + 1..].trim();

            match key {
                "description" | "info" | "esid" | "es5id" => {
                    let val = parse_scalar_value(value_part, &lines, &mut idx);
                    match key {
                        "description" => fm.description = Some(val),
                        "info" => fm.info = Some(val),
                        _ => {
                            // esid / es5id — normalize to esid field.
                            if fm.esid.is_none() {
                                fm.esid = Some(val);
                            }
                        }
                    }
                    idx += 1;
                }
                "features" | "flags" | "includes" => {
                    let items = parse_string_list(value_part, &lines, &mut idx);
                    match key {
                        "features" => fm.features = items,
                        "flags" => fm.flags = items,
                        "includes" => fm.includes = items,
                        _ => {}
                    }
                    idx += 1;
                }
                "negative" => {
                    fm.negative = parse_negative_block(value_part, &lines, &mut idx);
                    idx += 1;
                }
                // Known but ignored keys (author, defines, etc.)
                _ => {
                    // Consume any indented follow-up lines so we don't mis-parse
                    // them as top-level keys.
                    let mut j = idx + 1;
                    while j < lines.len()
                        && (lines[j].starts_with(' ') || lines[j].starts_with('\t'))
                    {
                        j += 1;
                    }
                    idx = j;
                    continue;
                }
            }
        } else {
            idx += 1;
        }
    }

    fm
}

/// Parses a scalar that may be inline or a block (`|` / `>`).
fn parse_scalar_value(value_part: &str, lines: &[&str], idx: &mut usize) -> String {
    if value_part.is_empty() || value_part == "|" || value_part == ">" || value_part == "|-" || value_part == ">-" {
        // Block scalar — collect indented lines.
        let mut out = String::new();
        let mut j = *idx + 1;
        while j < lines.len() {
            let l = lines[j];
            if l.trim().is_empty() {
                out.push('\n');
                j += 1;
                continue;
            }
            if l.starts_with(' ') || l.starts_with('\t') {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(l.trim());
                j += 1;
            } else {
                break;
            }
        }
        *idx = j - 1;
        if out.is_empty() {
            String::new()
        } else {
            out
        }
    } else {
        // Strip surrounding quotes if present.
        let v = value_part.trim();
        if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
            || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
        {
            v[1..v.len() - 1].to_string()
        } else {
            v.to_string()
        }
    }
}

/// Parses a string list that may be inline `[a, b]` or a block `- a\n- b`.
fn parse_string_list(value_part: &str, lines: &[&str], idx: &mut usize) -> Vec<String> {
    if value_part.starts_with('[') {
        // Inline array: [a, b, c]
        let inner = value_part
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim();
        if inner.is_empty() {
            return Vec::new();
        }
        return inner
            .split(',')
            .map(|s| {
                s.trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect();
    }

    if value_part.is_empty() {
        // Block array: next lines are `- item`.
        let mut items = Vec::new();
        let mut j = *idx + 1;
        while j < lines.len() {
            let l = lines[j].trim();
            if l.starts_with("- ") || l == "-" {
                let item = l.trim_start_matches("- ").trim_start_matches('-').trim();
                let cleaned = item.trim_matches('"').trim_matches('\'').to_string();
                if !cleaned.is_empty() {
                    items.push(cleaned);
                }
                j += 1;
            } else if l.is_empty() {
                j += 1;
            } else if lines[j].starts_with(' ') || lines[j].starts_with('\t') {
                // Indented non-list line — still part of block? skip.
                j += 1;
            } else {
                break;
            }
        }
        *idx = j - 1;
        return items;
    }

    // Single value without brackets (unlikely but handle).
    let cleaned = value_part.trim_matches('"').trim_matches('\'').to_string();
    if cleaned.is_empty() {
        Vec::new()
    } else {
        vec![cleaned]
    }
}

/// Parses the `negative:` block. Handles:
/// ```yaml
/// negative:
///   phase: parse
///   type: SyntaxError
/// ```
fn parse_negative_block(
    value_part: &str,
    lines: &[&str],
    idx: &mut usize,
) -> Option<Negative> {
    // Inline form: negative: {phase: parse, type: SyntaxError}
    // Rare, but handle simple case.
    if value_part.starts_with('{') {
        let inner = value_part.trim_matches(|c| c == '{' || c == '}');
        let mut phase = None;
        let mut type_name = None;
        for part in inner.split(',') {
            let kv: Vec<&str> = part.splitn(2, ':').collect();
            if kv.len() == 2 {
                let k = kv[0].trim();
                let v = kv[1].trim().trim_matches('"').trim_matches('\'');
                if k == "phase" {
                    phase = Some(v.to_string());
                } else if k == "type" {
                    type_name = Some(v.to_string());
                }
            }
        }
        if let (Some(p), Some(t)) = (phase, type_name) {
            return Some(Negative {
                phase: p,
                type_name: t,
            });
        }
        return None;
    }

    // Block form — look ahead for indented phase/type.
    let mut phase: Option<String> = None;
    let mut type_name: Option<String> = None;
    let mut j = *idx + 1;
    while j < lines.len() {
        let l = lines[j];
        if !l.starts_with(' ') && !l.starts_with('\t') {
            break;
        }
        let t = l.trim();
        if t.is_empty() {
            j += 1;
            continue;
        }
        if let Some(colon) = t.find(':') {
            let k = t[..colon].trim();
            let v = t[colon + 1..]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if k == "phase" {
                phase = Some(v);
            } else if k == "type" {
                type_name = Some(v);
            }
        }
        j += 1;
    }
    *idx = j - 1;
    match (phase, type_name) {
        (Some(p), Some(t)) => Some(Negative {
            phase: p,
            type_name: t,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_returns_none_when_no_frontmatter() {
        let src = "var x = 1;";
        assert!(extract_raw_frontmatter(src).is_none());
        let fm = parse_frontmatter(src);
        assert_eq!(fm, Frontmatter::default());
    }

    #[test]
    fn parse_simple_description_and_flags() {
        let src = r#"/*---
description: simple test
flags: [async]
---*/
var x = 1;"#;
        let fm = parse_frontmatter(src);
        assert_eq!(fm.description.as_deref(), Some("simple test"));
        assert_eq!(fm.flags, vec!["async"]);
    }

    #[test]
    fn parse_features_and_includes_inline() {
        let src = r#"/*---
description: check
features: [BigInt, Symbol]
includes: [assert.js, propertyHelper.js]
---*/"#;
        let fm = parse_frontmatter(src);
        assert_eq!(fm.features, vec!["BigInt", "Symbol"]);
        assert_eq!(fm.includes, vec!["assert.js", "propertyHelper.js"]);
    }

    #[test]
    fn parse_negative_parse_phase() {
        let src = r#"/*---
description: negative parse
negative:
  phase: parse
  type: SyntaxError
---*/
var x = ;"#;
        let fm = parse_frontmatter(src);
        let neg = fm.negative.expect("should have negative");
        assert_eq!(neg.phase, "parse");
        assert_eq!(neg.type_name, "SyntaxError");
    }

    #[test]
    fn parse_negative_runtime_phase() {
        let src = r#"/*---
description: runtime throw
negative:
  phase: runtime
  type: TypeError
---*/
throw new TypeError();"#;
        let fm = parse_frontmatter(src);
        let neg = fm.negative.unwrap();
        assert_eq!(neg.phase, "runtime");
        assert_eq!(neg.type_name, "TypeError");
    }

    #[test]
    fn has_flag_helper() {
        let src = r#"/*---
flags: [module, raw]
---*/"#;
        let fm = parse_frontmatter(src);
        assert!(fm.has_flag("module"));
        assert!(fm.has_flag("raw"));
        assert!(!fm.has_flag("async"));
    }

    #[test]
    fn strip_removes_block() {
        let src = "/*---\ndescription: a\n---*/var x = 1;";
        let stripped = strip_frontmatter(src);
        assert_eq!(stripped, "var x = 1;");
    }

    #[test]
    fn strip_no_block_returns_original() {
        let src = "var x = 1;";
        assert_eq!(strip_frontmatter(src), src);
    }

    #[test]
    fn parse_block_scalar_description() {
        let src = "/*---\ndescription: |\n   line one\n    line two\n---*/";
        let fm = parse_frontmatter(src);
        let d = fm.description.unwrap();
        assert!(d.contains("line one"));
        assert!(d.contains("line two"));
    }

    #[test]
    fn parse_block_array_features() {
        let src = "/*---\nfeatures:\n  - BigInt\n  - Symbol\n---*/";
        let fm = parse_frontmatter(src);
        assert_eq!(fm.features, vec!["BigInt", "Symbol"]);
    }

    #[test]
    fn resolution_phase_parsed() {
        let src = "/*---\nnegative:\n  phase: resolution\n  type: SyntaxError\nflags: [module]\n---*/";
        let fm = parse_frontmatter(src);
        assert_eq!(fm.negative.as_ref().unwrap().phase, "resolution");
        assert!(fm.has_flag("module"));
    }

    #[test]
    fn empty_arrays_handled() {
        let src = "/*---\nfeatures: []\nflags: []\n---*/";
        let fm = parse_frontmatter(src);
        assert!(fm.features.is_empty());
        assert!(fm.flags.is_empty());
    }
}
