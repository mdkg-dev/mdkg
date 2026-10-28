//! Pure edits of one configuration layer. No file writes.

use serde_json::Value;

use super::{Error, Input, Layer, MAX_BYTES, MAX_DEPTH, Origin, check_depth};

/// JSON scalars/containers are typed. Other bare words are strings. Malformed
/// JSON-looking input is refused; callers can quote a numeric-looking string.
pub fn value(origin: &Origin, text: &str) -> Result<Value, Error> {
    if text.len() as u64 > MAX_BYTES {
        return Err(Error::large(origin));
    }
    let parsed = match super::json::parse(text.as_bytes()) {
        Ok(value) => value,
        Err(_)
            if text
                .trim_start()
                .chars()
                .next()
                .is_some_and(|c| matches!(c, '{' | '[' | '"' | '-') || c.is_ascii_digit()) =>
        {
            return Err(Error::invalid(
                origin,
                "setting value must be valid JSON without duplicate keys",
            ));
        }
        Err(_) => Value::String(text.to_string()),
    };
    check_depth(&parsed, 0, origin)?;
    Ok(parsed)
}

fn parts(origin: &Origin, key: &str) -> Result<Vec<String>, Error> {
    let fail = || {
        Error::invalid(
            origin,
            "setting key must be a dotted object path or JSON pointer",
        )
    };
    if key.is_empty() || key.len() > 4096 || key.chars().any(char::is_control) {
        return Err(fail());
    }
    let parts = if let Some(pointer) = key.strip_prefix('/') {
        pointer
            .split('/')
            .map(|part| {
                let mut out = String::new();
                let mut chars = part.chars();
                while let Some(c) = chars.next() {
                    out.push(if c == '~' {
                        match chars.next() {
                            Some('0') => '~',
                            Some('1') => '/',
                            _ => return Err(fail()),
                        }
                    } else {
                        c
                    });
                }
                Ok(out)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let parts = key.split('.').map(str::to_owned).collect::<Vec<_>>();
        if parts.iter().any(String::is_empty) {
            return Err(fail());
        }
        parts
    };
    if parts.len() > MAX_DEPTH {
        return Err(fail());
    }
    Ok(parts)
}

/// Change one object key, retaining every unrelated key. Arrays are replaced
/// as whole values. Identity belongs to init/migration, never to this setter.
pub fn set(input: &Input, key: &str, value: Value) -> Result<Input, Error> {
    if !matches!(
        input.origin.layer,
        Layer::Project | Layer::User | Layer::Local
    ) {
        return Err(Error::invalid(
            &input.origin,
            "only project, user and local layers can be edited",
        ));
    }
    let mut path = parts(&input.origin, key)?;
    if matches!(
        path[0].as_str(),
        "schema_version" | "format_version" | "graph_id"
    ) {
        return Err(Error::invalid(
            &input.origin,
            "project identity is managed by init and migration",
        ));
    }
    let mut result = Input::new(input.origin.clone(), input.value.clone())?;
    let mut value = value;
    let storage = path.len() == 2
        && path[0] == "work"
        && matches!(path[1].as_str(), "location" | "committed");
    if storage {
        let location = match (path[1].as_str(), value.as_str()) {
            ("location", Some("local")) | ("committed", Some("none")) => "local",
            ("location", Some("git")) | ("committed", Some("definitions")) => "git",
            _ => {
                return Err(Error::invalid(
                    &input.origin,
                    "work storage must use local/git or its none/definitions alias",
                ));
            }
        };
        path[1] = "location".into();
        value = Value::String(location.into());
    }
    let mut at = &mut result.value;
    for part in &path[..path.len() - 1] {
        let map = at.as_object_mut().ok_or_else(|| {
            Error::invalid(&input.origin, "setting path crosses a non-object value")
        })?;
        at = map
            .entry(part.clone())
            .or_insert_with(|| Value::Object(Default::default()));
    }
    let map = at
        .as_object_mut()
        .ok_or_else(|| Error::invalid(&input.origin, "setting path crosses a non-object value"))?;
    if storage {
        map.remove("committed");
    }
    map.insert(path.last().expect("nonempty key").clone(), value);
    result.check()?;
    Ok(result)
}

#[cfg(test)]
mod tests;
