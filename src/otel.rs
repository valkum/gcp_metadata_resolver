//! Conversions into OpenTelemetry SDK types, behind the `opentelemetry`
//! feature.

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;

use crate::GcpResourceAttributes;

impl From<&GcpResourceAttributes> for Vec<KeyValue> {
    fn from(attributes: &GcpResourceAttributes) -> Self {
        attributes
            .attributes()
            .into_iter()
            .map(|(key, value)| KeyValue::new(key, value.to_owned()))
            .collect()
    }
}

impl From<&GcpResourceAttributes> for Resource {
    fn from(attributes: &GcpResourceAttributes) -> Self {
        Resource::builder()
            .with_attributes(Vec::<KeyValue>::from(attributes))
            .build()
    }
}
