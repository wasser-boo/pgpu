//! CLI search predicates → Vast JSON. Supports comparisons and `in`/`notin`
//! lists, including quoted names and whitespace inside lists. Malformed input
//! is an error, never a silently dropped location/hardware restriction.
//! RAM predicates use GB (Vast expects MB); aliases match the Vast CLI.

use anyhow::{ensure, Context, Result};
use serde_json::{Map, Value};

const OPS: &[(&str, &str)] = &[
    ("<=", "lte"),
    (">=", "gte"),
    ("!=", "neq"),
    ("==", "eq"),
    ("=", "eq"),
    ("<", "lt"),
    (">", "gt"),
];

/// Split only outside quotes/lists. The same lexer handles predicates and
/// comma-separated list items; empty list items are retained for validation.
fn split(input: &str, separator: impl Fn(char) -> bool) -> Result<Vec<&str>> {
    let (mut quote, mut escaped, mut bracket, mut start) = (None, false, false, 0);
    let mut parts = Vec::new();
    for (i, c) in input.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' && q == '"' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '[' => {
                ensure!(!bracket, "nested search lists are not supported");
                bracket = true;
            }
            ']' => {
                ensure!(bracket, "unexpected closing bracket");
                bracket = false;
            }
            _ if !bracket && separator(c) => {
                parts.push(input[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => (),
        }
    }
    ensure!(quote.is_none(), "unterminated quoted search value");
    ensure!(!bracket, "unterminated search list");
    parts.push(input[start..].trim());
    Ok(parts)
}

fn prefix_op(value: &str) -> Option<(&'static str, &str)> {
    OPS.iter()
        .find_map(|(symbol, name)| value.strip_prefix(symbol).map(|rest| (*name, rest)))
}

pub fn parse_query(query: &str) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    let tokens = split(query, char::is_whitespace)?;
    let mut tokens = tokens.into_iter().filter(|s| !s.is_empty());
    while let Some(token) = tokens.next() {
        let (field, op, value) = if let Some(i) = token.find(['=', '!', '<', '>']) {
            let (field, rest) = token.split_at(i);
            let (op, value) = prefix_op(rest).context("invalid comparison operator")?;
            let value = if value.is_empty() {
                tokens.next().context("missing search value")?
            } else {
                value
            };
            (field, op, value)
        } else {
            let next = tokens.next().context("missing search value/operator")?;
            if matches!(next, "in" | "notin") {
                (token, next, tokens.next().context("missing search list")?)
            } else if let Some((op, value)) = prefix_op(next) {
                let value = if value.is_empty() {
                    tokens.next().context("missing search value")?
                } else {
                    value
                };
                (token, op, value)
            } else {
                (token, "eq", next) // Historical implicit equality: field value.
            }
        };
        ensure!(
            field.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "invalid search field"
        );
        let field = match field {
            "cuda_vers" => "cuda_max_good",
            "dph" => "dph_total",
            other => other,
        };
        let value = if matches!(op, "in" | "notin") {
            let inner = value
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .context("in/notin requires a bracketed list")?;
            ensure!(
                !inner.trim().is_empty(),
                "empty search lists are not allowed"
            );
            Value::Array(
                split(inner, |c| c == ',')?
                    .into_iter()
                    .map(|item| coerce(field, item))
                    .collect::<Result<Vec<_>>>()?,
            )
        } else {
            coerce(field, value)?
        };
        let operators = out
            .entry(field.to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("search predicates are objects");
        // Keep both sides of a range; never overwrite an earlier restriction.
        ensure!(
            !operators.contains_key(op),
            "duplicate {op} operator for {field}"
        );
        operators.insert(op.to_owned(), value);
    }
    Ok(out)
}

fn coerce(field: &str, raw: &str) -> Result<Value> {
    let raw = raw.trim();
    let value = if raw.starts_with('"') {
        serde_json::from_str::<String>(raw).context("invalid quoted search value")?
    } else if raw.starts_with('\'') {
        let inner = raw
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .context("invalid single-quoted search value")?;
        ensure!(!inner.contains('\''), "invalid single-quoted search value");
        inner.to_owned()
    } else {
        ensure!(
            !raw.chars().any(|c| c.is_whitespace()
                || matches!(c, '[' | ']' | ',' | '\'' | '"' | '=' | '!' | '<' | '>')),
            "invalid scalar search value"
        );
        raw.to_owned()
    };
    ensure!(!value.is_empty(), "empty search value/list item");
    let multiplier: i64 = match field {
        "cpu_ram" | "gpu_ram" | "gpu_total_ram" => 1000,
        _ => 1,
    };
    if let Ok(n) = value.parse::<i64>() {
        return Ok(Value::from(
            n.checked_mul(multiplier)
                .context("RAM search value overflow")?,
        ));
    }
    if let Ok(n) = value.parse::<f64>() {
        let n = n * multiplier as f64;
        ensure!(n.is_finite(), "search numbers must be finite");
        return Ok(serde_json::json!(n));
    }
    match value.as_str() {
        "true" | "True" => Ok(Value::Bool(true)),
        "false" | "False" => Ok(Value::Bool(false)),
        _ => Ok(Value::String(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_queries_remain_compatible() {
        let q = parse_query(r#"gpu_ram>=16 cpu_ram>=32 gpu_name="RTX 3090" reliability2>=0.95 inet_down>=800 disk_bw>=1500 cuda_vers>=12.9 dph<0.2"#).unwrap();
        assert_eq!(q["gpu_ram"]["gte"], json!(16000));
        assert_eq!(q["cpu_ram"]["gte"], json!(32000));
        assert_eq!(q["gpu_name"]["eq"], json!("RTX 3090"));
        assert_eq!(q["reliability2"]["gte"], json!(0.95));
        assert_eq!(q["inet_down"]["gte"], json!(800));
        assert_eq!(q["disk_bw"]["gte"], json!(1500));
        assert_eq!(q["cuda_max_good"]["gte"], json!(12.9));
        assert_eq!(q["dph_total"]["lt"], json!(0.2));
    }

    #[test]
    fn whitespace_implicit_equality_and_quotes() {
        for query in [
            "gpu_ram>=24",
            "gpu_ram >= 24",
            "gpu_ram>= 24",
            "gpu_ram >=24",
            "gpu_ram\t>=\n24",
        ] {
            assert_eq!(parse_query(query).unwrap()["gpu_ram"]["gte"], json!(24000));
        }
        assert_eq!(
            parse_query("gpu_name RTX_3090").unwrap()["gpu_name"]["eq"],
            json!("RTX_3090")
        );
        assert_eq!(
            parse_query("gpu_name='RTX 4090'").unwrap()["gpu_name"]["eq"],
            json!("RTX 4090")
        );
        assert!(parse_query(" \n\t ").unwrap().is_empty());
    }

    #[test]
    fn germany_and_neighbours_preserve_other_filters() {
        let q = parse_query("gpu_ram>=16 cpu_ram>=48 num_gpus=1 disk_space>=120 inet_down>=1000 cpu_cores_effective>=8 reliability2>=0.90 geolocation in [DE,AT,CH,NL,BE,LU,FR,CZ,PL,DK]").unwrap();
        assert_eq!(
            q["geolocation"],
            json!({"in":["DE","AT","CH","NL","BE","LU","FR","CZ","PL","DK"]})
        );
        assert_eq!(q["cpu_ram"]["gte"], json!(48000));
        assert_eq!(q["disk_space"]["gte"], json!(120));
        assert_eq!(q["num_gpus"]["eq"], json!(1));
        assert_eq!(q.len(), 8);
    }

    #[test]
    fn lists_support_spaces_quotes_and_notin() {
        let q = parse_query(r#"geolocation in [ "DE", 'AT', CH ] gpu_name in ["RTX 4090", "RTX 3090"] machine_id notin [12, 34]"#).unwrap();
        assert_eq!(q["geolocation"]["in"], json!(["DE", "AT", "CH"]));
        assert_eq!(q["gpu_name"]["in"], json!(["RTX 4090", "RTX 3090"]));
        assert_eq!(q["machine_id"]["notin"], json!([12, 34]));
        assert_eq!(
            parse_query("gpu_ram in [16, 24]").unwrap()["gpu_ram"]["in"],
            json!([16000, 24000])
        );
    }

    #[test]
    fn commas_and_operators_inside_quoted_names_are_not_syntax() {
        let q = parse_query(r#"gpu_name in ["a,b", "x>=y", "A \"GPU\""]"#).unwrap();
        assert_eq!(q["gpu_name"]["in"], json!(["a,b", "x>=y", "A \"GPU\""]));
        assert_eq!(
            parse_query(r#"gpu_name="x>=y""#).unwrap()["gpu_name"]["eq"],
            json!("x>=y")
        );
    }

    #[test]
    fn range_bounds_merge_and_booleans_are_typed() {
        let q = parse_query("gpu_ram>=16 gpu_ram<48 verified=true rented=False").unwrap();
        assert_eq!(q["gpu_ram"], json!({"gte":16000,"lt":48000}));
        assert_eq!(q["verified"]["eq"], json!(true));
        assert_eq!(q["rented"]["eq"], json!(false));
    }

    #[test]
    fn country_exclusions_are_preserved_alongside_allowlists() {
        let q = parse_query("geolocation in [DE,AT,FR] geolocation!=DE geolocation notin [FR,CH]")
            .unwrap();
        assert_eq!(
            q["geolocation"],
            json!({"in":["DE","AT","FR"],"neq":"DE","notin":["FR","CH"]})
        );
        assert_eq!(
            parse_query("geolocation!=DE").unwrap()["geolocation"]["neq"],
            json!("DE")
        );
    }

    #[test]
    fn malformed_filters_fail_closed() {
        for query in [
            "geolocation",
            "geolocation in",
            "geolocation in DE",
            "geolocation in []",
            "geolocation in [DE,]",
            "geolocation in [,DE]",
            "geolocation in [DE,,AT]",
            "geolocation in [DE AT]",
            "geolocation in [[DE]]",
            "geolocation in [DE",
            "geolocation in [DE]]",
            "geolocation in [\"DE]",
            "geolocation in [\"\"]",
            "geolocation in [DE] geolocation in [US]",
            "geolocation=DE geolocation=US",
            "geolocation not in [DE]",
            "gpu_name=\"RTX\"oops",
            "gpu_name=RTX\"",
            "gpu_ram >=",
            "gpu_ram!16",
            "gpu_ram=>16",
            "gpu_ram=1e999",
            "gpu_ram=9223372036854775807",
            "reliability2=NaN",
            "[bad]=DE",
        ] {
            assert!(
                parse_query(query).is_err(),
                "accepted invalid query: {query}"
            );
        }
    }
}
