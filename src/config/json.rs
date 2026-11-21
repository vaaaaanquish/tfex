//! JSON-syntax (`.tf.json`) files.
//!
//! JSON carries no positions, so every declaration is reported on line 1.

use serde_json::{Map, Value};

use super::{Backend, Configuration, ModuleCall, Resource, ResourceKind, is_safe_backend_setting};

type Object = Map<String, Value>;

/// The line reported for every declaration.
const LINE: usize = 1;

/// Add the declarations in `text` to `configuration`.
///
/// Anything that does not have the shape Terraform expects is ignored.
pub(super) fn read(text: &str, file: &str, configuration: &mut Configuration) {
    let Ok(Value::Object(root)) = serde_json::from_str(text) else {
        return;
    };
    for (key, kind) in [
        ("resource", ResourceKind::Managed),
        ("data", ResourceKind::Data),
    ] {
        for (type_name, names) in entries(root.get(key)) {
            for (name, _) in entries(Some(names)) {
                configuration
                    .resources
                    .push(Resource::new(kind, type_name, name, file, LINE));
            }
        }
    }
    for (name, arguments) in entries(root.get("module")) {
        configuration.module_calls.push(ModuleCall {
            name: name.clone(),
            source: objects(Some(arguments))
                .find_map(|arguments| arguments.get("source"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            file: file.to_owned(),
            line: LINE,
        });
    }
    if entries(root.get("provider")).next().is_some() {
        configuration.has_provider = true;
    }
    let backend = objects(root.get("terraform"))
        .flat_map(|terraform| entries(terraform.get("backend")))
        .next();
    if let Some((kind, settings)) = backend {
        configuration.backend = Some(Backend {
            kind: kind.clone(),
            settings: entries(Some(settings))
                .filter(|(name, value)| is_safe_backend_setting(name) && is_scalar(value))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        });
    }
}

/// The objects at a block position. Terraform's JSON syntax accepts either a
/// single object or a list of objects wherever a block may appear.
fn objects(value: Option<&Value>) -> impl Iterator<Item = &Object> {
    let (single, list) = match value {
        Some(Value::Object(object)) => (Some(object), None),
        Some(Value::Array(items)) => (None, Some(items)),
        _ => (None, None),
    };
    single
        .into_iter()
        .chain(list.into_iter().flatten().filter_map(Value::as_object))
}

/// The key/value pairs of every object at a block position.
fn entries(value: Option<&Value>) -> impl Iterator<Item = (&String, &Value)> {
    objects(value).flatten()
}

fn is_scalar(value: &Value) -> bool {
    value.is_string() || value.is_number() || value.is_boolean()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(text: &str) -> Configuration {
        let mut configuration = Configuration::default();
        read(text, "main.tf.json", &mut configuration);
        configuration
    }

    fn addresses(configuration: &Configuration) -> Vec<String> {
        let mut addresses: Vec<String> = configuration
            .resources
            .iter()
            .map(Resource::address)
            .collect();
        addresses.sort();
        addresses
    }

    #[test]
    fn reads_object_form() {
        let configuration = parse(
            r#"{
  "resource": {"aws_s3_bucket": {"logs": {}, "assets": {}}},
  "data": {"aws_region": {"current": {}}},
  "module": {"network": {"source": "./modules/network"}},
  "provider": {"aws": {"region": "us-east-1"}},
  "terraform": {
    "backend": {"gcs": {"bucket": "state", "prefix": "prod", "credentials": "SECRET"}}
  }
}"#,
        );
        assert_eq!(
            addresses(&configuration),
            [
                "aws_s3_bucket.assets",
                "aws_s3_bucket.logs",
                "data.aws_region.current"
            ]
        );
        assert!(
            configuration
                .resources
                .iter()
                .all(|resource| resource.line == LINE)
        );
        assert_eq!(
            configuration.module_calls[0].source.as_deref(),
            Some("./modules/network")
        );
        assert!(configuration.has_provider);
        let backend = configuration.backend.unwrap();
        assert_eq!(backend.kind, "gcs");
        assert_eq!(
            json!(backend.settings),
            json!({"bucket": "state", "prefix": "prod"})
        );
    }

    #[test]
    fn reads_list_form() {
        let configuration = parse(
            r#"{
  "resource": [{"null_resource": [{"a": {}}, {"b": {}}]}],
  "module": [{"network": [{"source": "./network"}]}],
  "terraform": [{"backend": [{"s3": [{"bucket": "state", "encrypt": true}]}]}]
}"#,
        );
        assert_eq!(
            addresses(&configuration),
            ["null_resource.a", "null_resource.b"]
        );
        assert_eq!(
            configuration.module_calls[0].source.as_deref(),
            Some("./network")
        );
        assert_eq!(
            json!(configuration.backend.unwrap().settings),
            json!({"bucket": "state"})
        );
    }

    #[test]
    fn ignores_malformed_input() {
        for text in [
            "[1, 2",
            "[]",
            r#"{"resource": "nope", "module": [3], "provider": {}, "terraform": 1}"#,
        ] {
            let configuration = parse(text);
            assert!(configuration.resources.is_empty(), "{text}");
            assert!(configuration.module_calls.is_empty(), "{text}");
            assert!(!configuration.has_provider, "{text}");
            assert_eq!(configuration.backend, None, "{text}");
        }
    }
}
