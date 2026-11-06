//! Exact PostgreSQL privilege-list syntax for the supported privilege helpers.
use fedengine_core::{EngineError, Result};

pub fn parse_privileges(kind: &str, input: &str) -> Result<Vec<String>> {
    let valid: &[&str] = match kind {
        "database" => &["CONNECT", "CREATE", "TEMP", "TEMPORARY"],
        "schema" => &["USAGE", "CREATE"],
        "table" => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
        ],
        _ => {
            return Err(EngineError::unsupported(
                "unsupported privilege object kind",
            ));
        }
    };
    input
        .split(',')
        .map(|part| {
            let normalized = part
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_uppercase();
            let (name, grant_option) = normalized
                .strip_suffix(" WITH GRANT OPTION")
                .map(|name| (name, true))
                .unwrap_or((&normalized, false));
            if !valid.contains(&name) {
                return Err(EngineError::new("22023", "unrecognized privilege type"));
            }
            let name = if name == "TEMP" { "TEMPORARY" } else { name };
            Ok(format!(
                "{name}{}",
                if grant_option {
                    " WITH GRANT OPTION"
                } else {
                    ""
                }
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_privilege_list_syntax() {
        assert_eq!(
            parse_privileges("schema", "usage, CREATE with grant option").unwrap(),
            ["USAGE", "CREATE WITH GRANT OPTION"]
        );
        assert_eq!(parse_privileges("database", "temp").unwrap(), ["TEMPORARY"]);
        for value in ["NOT_USAGE", "", "USAGE,", "USAGE OTHER", "USAGE WITH GRANT"] {
            assert_eq!(
                parse_privileges("schema", value).unwrap_err().sqlstate,
                "22023"
            );
        }
    }
}
