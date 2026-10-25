use quartz_nbt::NbtTag;
use serde_json::Value;

pub fn nbt_to_display_value(tag: NbtTag) -> Value {
  match tag {
    NbtTag::Compound(data) => Value::Object(
      data
        .into_inner()
        .into_iter()
        .map(|(key, value)| (key, nbt_to_display_value(value)))
        .collect(),
    ),
    NbtTag::List(data) => Value::Array(
      data
        .into_inner()
        .into_iter()
        .map(nbt_to_display_value)
        .collect(),
    ),
    NbtTag::Byte(value) => value.into(),
    NbtTag::Short(value) => value.into(),
    NbtTag::Int(value) => value.into(),
    // Preserve seeds and other large integers across the JavaScript boundary.
    NbtTag::Long(value) if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&value) => {
      value.to_string().into()
    }
    NbtTag::Long(value) => value.into(),
    NbtTag::Float(value) if !value.is_finite() => value.to_string().into(),
    NbtTag::Float(value) => value.into(),
    NbtTag::Double(value) if !value.is_finite() => value.to_string().into(),
    NbtTag::Double(value) => value.into(),
    NbtTag::String(value) => value.into(),
    NbtTag::ByteArray(values) => values.into(),
    NbtTag::IntArray(values) => values.into(),
    NbtTag::LongArray(values) => Value::Array(
      values
        .into_iter()
        .map(|value| nbt_to_display_value(NbtTag::Long(value)))
        .collect(),
    ),
  }
}

#[cfg(test)]
mod tests {
  use crate::utils::nbt::nbt_to_display_value;
  use quartz_nbt::NbtTag;

  #[test]
  fn preserves_large_integers_and_non_finite_numbers() {
    assert_eq!(
      nbt_to_display_value(NbtTag::Long(i64::MAX)),
      i64::MAX.to_string()
    );
    assert_eq!(nbt_to_display_value(NbtTag::Long(42)), 42);
    assert_eq!(nbt_to_display_value(NbtTag::Double(f64::INFINITY)), "inf");
    assert_eq!(
      nbt_to_display_value(NbtTag::LongArray(vec![42, i64::MIN])),
      serde_json::json!([42, i64::MIN.to_string()])
    );
  }
}
