//! Query-Parser: CLI-Syntax (`gpu_ram>=16 gpu_name="RTX 3090"`) →
//! Vast-JSON (`{"gpu_ram": {"gte": 16000}, "gpu_name": {"eq": "RTX 3090"}}`).
//!
//! Wie das offizielle vastai-SDK: cpu_ram/gpu_ram/gpu_total_ram werden mit
//! 1000 multipliziert (Query in GB, API will MB); Aliases (dph, cuda_vers).

const OPS: &[&str] = &["<=", ">=", "!=", "==", "=", "<", ">"];

/// Whitespace-Tokenizer mit Rücksicht auf "quoted values".
fn tokenize(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in query.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

pub fn parse_query(query: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let tokens: Vec<String> = tokenize(query);
    let tokens: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();
    let mut i = 0usize;
    while i < tokens.len() {
        let t = tokens[i];
        i += 1;
        // 1) Klebend: field>=value
        if let Some((field, op, value)) = glued(t) {
            insert(&mut out, &field, &op, &value);
            continue;
        }
        // 2) field [op] value
        let field = t.to_string();
        if i >= tokens.len() {
            break;
        }
        let next = tokens[i];
        if let Some(op) = standalone_op(next) {
            i += 1;
            if i >= tokens.len() {
                break;
            }
            let value = tokens[i];
            i += 1;
            insert(&mut out, &field, &op, &value);
        } else {
            let value = next;
            i += 1;
            insert(&mut out, &field, "eq", value);
        }
    }
    out
}

fn glued(token: &str) -> Option<(String, String, String)> {
    for sep in ["<=", ">=", "!=", "==", "<", ">", "="] {
        if let Some(pos) = token.find(sep) {
            let field = token[..pos].to_string();
            if field.is_empty() {
                return None;
            }
            let value = token[pos + sep.len()..].to_string();
            if value.is_empty() {
                return None;
            }
            return Some((field, canonical(sep), value));
        }
    }
    None
}

fn standalone_op(token: &str) -> Option<String> {
    if token == "=" || token == "==" {
        return Some("eq".into());
    }
    if OPS.contains(&token) {
        Some(canonical(token))
    } else {
        None
    }
}

fn canonical(op: &str) -> String {
    match op {
        "<=" => "lte".into(),
        ">=" => "gte".into(),
        "<" => "lt".into(),
        ">" => "gt".into(),
        "!=" => "neq".into(),
        _ => "eq".into(),
    }
}

fn insert(out: &mut serde_json::Map<String, serde_json::Value>, raw_field: &str, op: &str, raw_value: &str) {
    let field = alias(raw_field);
    let value = coerce(field.as_str(), raw_value);
    out.insert(field, serde_json::json!({ op: value }));
}

fn alias(field: &str) -> String {
    match field {
        "cuda_vers" => "cuda_max_good".into(),
        "dph" => "dph_total".into(),
        other => other.to_string(),
    }
}

/// Zahl wenn möglich (GB→MB-Multiplikator für RAM-Felder), sonst String.
fn coerce(field: &str, raw: &str) -> serde_json::Value {
    let raw_trim = raw.trim().trim_matches(|c| c == '"' || c == '\'');
    let mult = match field {
        "cpu_ram" | "gpu_ram" | "gpu_total_ram" => 1000.0,
        _ => 1.0,
    };
    if let Ok(n) = raw_trim.parse::<i64>() {
        if mult == 1.0 {
            return serde_json::json!(n);
        }
        return serde_json::json!(n * mult as i64);
    }
    if let Ok(f) = raw_trim.parse::<f64>() {
        return serde_json::json!(f * mult);
    }
    serde_json::json!(raw_trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gte_with_multiplier() {
        let q = parse_query("gpu_ram>=16 cpu_ram>=32");
        assert_eq!(q["gpu_ram"]["gte"], serde_json::json!(16000));
        assert_eq!(q["cpu_ram"]["gte"], serde_json::json!(32000));
    }

    #[test]
    fn parses_quoted_eq() {
        let q = parse_query(r#"gpu_name="RTX 3090""#);
        assert_eq!(q["gpu_name"]["eq"], serde_json::json!("RTX 3090"));
    }

    #[test]
    fn parses_plain_numbers_and_floats() {
        let q = parse_query("reliability2>=0.95 inet_down>=800 disk_bw>=1500");
        assert_eq!(q["reliability2"]["gte"], serde_json::json!(0.95));
        assert_eq!(q["inet_down"]["gte"], serde_json::json!(800));
        assert_eq!(q["disk_bw"]["gte"], serde_json::json!(1500));
    }

    #[test]
    fn aliases() {
        let q = parse_query("cuda_vers>=12.9");
        assert_eq!(q["cuda_max_good"]["gte"], serde_json::json!(12.9));
    }

    #[test]
    fn spaced_ops() {
        let q = parse_query("gpu_ram >= 24  gpu_name = \"RTX 4090\"");
        assert_eq!(q["gpu_ram"]["gte"], serde_json::json!(24000));
        assert_eq!(q["gpu_name"]["eq"], serde_json::json!("RTX 4090"));
    }

    #[test]
    fn implicit_eq() {
        let q = parse_query("gpu_name=RTX_3090");
        assert_eq!(q["gpu_name"]["eq"], serde_json::json!("RTX_3090"));
    }
}