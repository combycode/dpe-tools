//! Dotted-path lookup on serde_json::Value.  Strips the leading "v." /
//! "env." root segment so callers can write natural paths.

use serde_json::Value;

pub fn resolve<'a>(env: &'a Value, path: &str) -> Option<&'a Value> {
    let (base, segs) = parse_with_root(env, path);
    if segs.is_empty() && path == "v" {
        return env.get("v");
    }
    if segs.is_empty() && path == "env" {
        return Some(env);
    }
    let mut cur = base;
    for p in &segs { cur = cur.get(p)?; }
    Some(cur)
}

/// Return (base, segments) deciding the correct scope root.
fn parse_with_root<'a>(env: &'a Value, path: &str) -> (&'a Value, Vec<String>) {
    if let Some(rest) = path.strip_prefix("env.") {
        return (env, split(rest));
    }
    if let Some(rest) = path.strip_prefix("v.") {
        return (env.get("v").unwrap_or(env), split(rest));
    }
    if path == "v" { return (env.get("v").unwrap_or(env), Vec::new()); }
    if path == "env" { return (env, Vec::new()); }
    // No prefix: default to v.
    (env.get("v").unwrap_or(env), split(path))
}

fn split(s: &str) -> Vec<String> {
    if s.is_empty() { return Vec::new(); }
    s.split('.').map(|s| s.to_string()).collect()
}

pub fn resolve_owned(env: &Value, path: &str) -> Value {
    resolve(env, path).cloned().unwrap_or(Value::Null)
}

pub fn parse(path: &str) -> Vec<String> {
    if path.is_empty() { return Vec::new(); }
    let stripped = if let Some(r) = path.strip_prefix("v.") { r }
                   else if let Some(r) = path.strip_prefix("env.") { r }
                   else if path == "v" || path == "env" { "" }
                   else { path };
    if stripped.is_empty() { return Vec::new(); }
    stripped.split('.').map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test] fn parse_strips_v_prefix() {
        assert_eq!(parse("v.a.b"), vec!["a", "b"]);
    }
    #[test] fn parse_strips_env_prefix() {
        assert_eq!(parse("env.id"), vec!["id"]);
    }
    #[test] fn parse_bare_root_empty() {
        assert!(parse("v").is_empty());
        assert!(parse("env").is_empty());
    }
    #[test] fn parse_no_prefix() {
        assert_eq!(parse("a.b"), vec!["a", "b"]);
    }
    #[test] fn parse_empty_string_empty() {
        assert!(parse("").is_empty());
    }

    #[test] fn resolve_nested() {
        let env = json!({"v": {"a": {"b": 42}}});
        assert_eq!(resolve(&env, "v.a.b").unwrap(), &json!(42));
    }
    #[test] fn resolve_missing_none() {
        let env = json!({"v": {"a": 1}});
        assert!(resolve(&env, "v.b").is_none());
    }
    #[test] fn resolve_env_level() {
        let env = json!({"t": "d", "id": "abc", "v": {}});
        assert_eq!(resolve(&env, "env.id").unwrap(), &json!("abc"));
    }
    #[test] fn resolve_owned_null_on_missing() {
        let env = json!({"v": {}});
        assert_eq!(resolve_owned(&env, "v.missing"), Value::Null);
    }
    #[test] fn resolve_owned_clones() {
        let env = json!({"v": {"x": "hi"}});
        assert_eq!(resolve_owned(&env, "v.x"), json!("hi"));
    }
}
