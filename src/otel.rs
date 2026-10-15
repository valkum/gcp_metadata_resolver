//! Conversions into OpenTelemetry SDK types, behind one feature per
//! OpenTelemetry version, e.g. `opentelemetry_0_33`.

/// Implements the conversions for one OpenTelemetry version. `$otel` and
/// `$sdk` are the renamed `opentelemetry` and `opentelemetry_sdk` crates.
macro_rules! impl_otel {
    ($feature:literal, $module:ident, $otel:ident, $sdk:ident) => {
        #[cfg(feature = $feature)]
        mod $module {
            use $otel::KeyValue;
            use $sdk::Resource;

            use crate::GcpResourceAttributes;

            #[cfg_attr(docsrs, doc(cfg(feature = $feature)))]
            impl From<&GcpResourceAttributes> for Vec<KeyValue> {
                fn from(attributes: &GcpResourceAttributes) -> Self {
                    attributes
                        .attributes()
                        .into_iter()
                        .map(|(key, value)| KeyValue::new(key, value.to_owned()))
                        .collect()
                }
            }

            #[cfg_attr(docsrs, doc(cfg(feature = $feature)))]
            impl From<&GcpResourceAttributes> for Resource {
                fn from(attributes: &GcpResourceAttributes) -> Self {
                    Resource::builder()
                        .with_attributes(Vec::<KeyValue>::from(attributes))
                        .build()
                }
            }

            #[cfg(test)]
            mod tests {
                use $otel::{Key, KeyValue, Value};
                use $sdk::Resource;

                use crate::GcpResourceAttributes;

                #[test]
                fn attributes_into_otel_resource() {
                    let mut attrs = GcpResourceAttributes::new("my-project".to_owned());
                    attrs.host_id = Some("1234567891".to_owned());

                    let key_values = Vec::<KeyValue>::from(&attrs);
                    assert_eq!(key_values.len(), attrs.attributes().len());

                    let resource = Resource::from(&attrs);
                    assert_eq!(
                        resource.get(&Key::from_static_str("cloud.provider")),
                        Some(Value::from("gcp"))
                    );
                    assert_eq!(
                        resource.get(&Key::from_static_str("host.id")),
                        Some(Value::from("1234567891"))
                    );
                    // Undetected fields must not reach the resource.
                    assert_eq!(resource.get(&Key::from_static_str("faas.name")), None);
                }
            }
        }
    };
}

impl_otel!(
    "opentelemetry_0_32",
    v0_32,
    opentelemetry_0_32,
    opentelemetry_sdk_0_32
);
impl_otel!(
    "opentelemetry_0_33",
    v0_33,
    opentelemetry_0_33,
    opentelemetry_sdk_0_33
);
