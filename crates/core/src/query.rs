//! Server-side filtering, projection, and ordering of decoded account JSON.
//!
//! Decoded accounts can be large and numerous. These helpers let callers ask
//! for only the accounts and fields they need, which keeps responses small for
//! humans and token-constrained AI agents alike.

use std::cmp::Ordering;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::codec::lookup_path;

/// Comparison applied by a [`Condition`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    /// Field equals the value.
    Eq,
    /// Field does not equal the value.
    Ne,
    /// Field is greater than the value.
    Gt,
    /// Field is greater than or equal to the value.
    Gte,
    /// Field is less than the value.
    Lt,
    /// Field is less than or equal to the value.
    Lte,
    /// String field contains the value as a substring, or array field contains the value.
    Contains,
    /// Field equals any element of the array value.
    In,
    /// Field is present and non-null when the value is true, absent or null when false.
    Exists,
}

/// One predicate over a dotted path into a decoded account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// Dotted field path with optional `[index]` segments, for example `state.kind` or `votes[0]`.
    pub path: String,
    /// Comparison operator.
    pub op: Operator,
    /// Comparison operand.
    #[serde(default)]
    pub value: Value,
}

/// Returns `true` when every condition holds for `value`.
///
/// Numeric comparisons accept JSON numbers and numeric strings, because the
/// codec renders 64-bit and wider integers as strings to avoid precision loss.
/// Enum values decode as `{"kind": NAME, ...}`, so a condition on a path that
/// resolves to such an object compares its `kind` when the operand is a string.
pub fn matches(value: &Value, conditions: &[Condition]) -> Result<bool> {
    for condition in conditions {
        if !condition_holds(value, condition)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn condition_holds(value: &Value, condition: &Condition) -> Result<bool> {
    let field = lookup_path(value, &condition.path)?;
    let present = field.is_some_and(|field| !field.is_null());
    if condition.op == Operator::Exists {
        let expected = condition.value.as_bool().unwrap_or(true);
        return Ok(present == expected);
    }
    let Some(field) = field else {
        return Ok(condition.op == Operator::Ne);
    };
    let field = enum_kind(field, &condition.value);
    Ok(match condition.op {
        Operator::Eq => compare(field, &condition.value) == Some(Ordering::Equal),
        Operator::Ne => compare(field, &condition.value) != Some(Ordering::Equal),
        Operator::Gt => compare(field, &condition.value) == Some(Ordering::Greater),
        Operator::Gte => matches!(
            compare(field, &condition.value),
            Some(Ordering::Greater | Ordering::Equal)
        ),
        Operator::Lt => compare(field, &condition.value) == Some(Ordering::Less),
        Operator::Lte => matches!(
            compare(field, &condition.value),
            Some(Ordering::Less | Ordering::Equal)
        ),
        Operator::Contains => match field {
            Value::String(text) => condition
                .value
                .as_str()
                .is_some_and(|needle| text.contains(needle)),
            Value::Array(items) => items
                .iter()
                .any(|item| compare(item, &condition.value) == Some(Ordering::Equal)),
            _ => false,
        },
        Operator::In => {
            let Some(options) = condition.value.as_array() else {
                bail!(
                    "condition on {} uses `in` but its value is not an array",
                    condition.path
                )
            };
            options
                .iter()
                .any(|option| compare(field, option) == Some(Ordering::Equal))
        }
        Operator::Exists => unreachable!("handled above"),
    })
}

/// Treats a decoded enum object as its variant name when compared against a string.
fn enum_kind<'a>(field: &'a Value, operand: &Value) -> &'a Value {
    if operand.is_string()
        && let Some(kind) = field.get("kind")
        && kind.is_string()
    {
        return kind;
    }
    field
}

/// Orders two JSON values, treating numeric strings as numbers.
pub fn compare(left: &Value, right: &Value) -> Option<Ordering> {
    if let (Some(left), Some(right)) = (as_number(left), as_number(right)) {
        return left.partial_cmp(&right);
    }
    match (left, right) {
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        _ => (left == right).then_some(Ordering::Equal),
    }
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            // Base58 addresses contain digits, so only treat purely numeric text as a number.
            if text
                .chars()
                .all(|ch| ch.is_ascii_digit() || matches!(ch, '-' | '+' | '.'))
            {
                text.parse().ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Keeps only the listed paths, returning an object keyed by each path.
///
/// Paths that do not resolve map to `null` so callers can see which fields
/// were absent rather than silently losing them.
pub fn project(value: &Value, fields: &[String]) -> Result<Value> {
    let mut out = Map::new();
    for field in fields {
        let selected = lookup_path(value, field)?.cloned().unwrap_or(Value::Null);
        out.insert(field.clone(), selected);
    }
    Ok(Value::Object(out))
}

/// Reduction applied by an [`Aggregate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateOp {
    /// Sum of numeric values.
    Sum,
    /// Smallest value and the key of the item holding it.
    Min,
    /// Largest value and the key of the item holding it.
    Max,
    /// Arithmetic mean of numeric values.
    Avg,
    /// Number of items where the path is present and non-null.
    Count,
}

/// One reduction over a path across many decoded values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    /// Reduction to apply.
    pub op: AggregateOp,
    /// Dotted field path to reduce.
    pub path: String,
}

/// Exact integer or floating accumulator, so 64-bit lamport sums never lose precision.
#[derive(Clone, Copy, Debug)]
enum Number {
    Int(i128),
    Float(f64),
}

impl Number {
    fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => number
                .as_i64()
                .map(|int| Self::Int(int.into()))
                .or_else(|| number.as_u64().map(|int| Self::Int(int.into())))
                .or_else(|| number.as_f64().map(Self::Float)),
            Value::String(text) => {
                let text = text.trim();
                text.parse::<i128>()
                    .ok()
                    .map(Self::Int)
                    .or_else(|| as_number(value).map(Self::Float))
            }
            _ => None,
        }
    }

    fn add(self, other: Self) -> Self {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => Self::Int(left + right),
            (left, right) => Self::Float(left.as_f64() + right.as_f64()),
        }
    }

    fn as_f64(self) -> f64 {
        match self {
            Self::Int(int) => int as f64,
            Self::Float(float) => float,
        }
    }

    /// Integers render as strings, matching how the codec renders wide integers.
    fn into_json(self) -> Value {
        match self {
            Self::Int(int) => Value::String(int.to_string()),
            Self::Float(float) => json!(float),
        }
    }
}

/// Reduces `(key, value)` pairs by the requested aggregates.
///
/// The result maps `"op(path)"` to the reduction. `sum`, `avg`, and `count`
/// produce a scalar; `min` and `max` produce `{"value", "key"}` so callers can
/// identify the extreme item. Non-numeric and missing values are skipped by
/// numeric reductions.
pub fn aggregate<'a>(
    items: impl IntoIterator<Item = (&'a str, &'a Value)>,
    aggregates: &[Aggregate],
) -> Result<Map<String, Value>> {
    struct State {
        sum: Option<Number>,
        count: usize,
        min: Option<(Number, String)>,
        max: Option<(Number, String)>,
        present: usize,
    }
    let mut states = aggregates
        .iter()
        .map(|_| State {
            sum: None,
            count: 0,
            min: None,
            max: None,
            present: 0,
        })
        .collect::<Vec<_>>();
    for (key, value) in items {
        for (spec, state) in aggregates.iter().zip(states.iter_mut()) {
            let Some(field) = lookup_path(value, &spec.path)? else {
                continue;
            };
            if field.is_null() {
                continue;
            }
            state.present += 1;
            let Some(number) = Number::parse(field) else {
                continue;
            };
            state.count += 1;
            state.sum = Some(state.sum.map_or(number, |sum| sum.add(number)));
            if state
                .min
                .as_ref()
                .is_none_or(|(current, _)| number.as_f64() < current.as_f64())
            {
                state.min = Some((number, key.to_owned()));
            }
            if state
                .max
                .as_ref()
                .is_none_or(|(current, _)| number.as_f64() > current.as_f64())
            {
                state.max = Some((number, key.to_owned()));
            }
        }
    }
    let mut out = Map::new();
    for (spec, state) in aggregates.iter().zip(states) {
        let label = format!(
            "{}({})",
            serde_json::to_value(spec.op)?.as_str().unwrap_or("?"),
            spec.path
        );
        let value = match spec.op {
            AggregateOp::Sum => state.sum.map_or(json!("0"), Number::into_json),
            AggregateOp::Avg => {
                if state.count == 0 {
                    Value::Null
                } else {
                    json!(state.sum.map_or(0.0, Number::as_f64) / state.count as f64)
                }
            }
            AggregateOp::Count => json!(state.present),
            AggregateOp::Min => state.min.map_or(
                Value::Null,
                |(number, key)| json!({"value": number.into_json(), "key": key}),
            ),
            AggregateOp::Max => state.max.map_or(
                Value::Null,
                |(number, key)| json!({"value": number.into_json(), "key": key}),
            ),
        };
        out.insert(label, value);
    }
    Ok(out)
}

/// Sorts decoded values by a path, placing missing fields last.
pub fn sort_by_path<T>(items: &mut [T], path: &str, descending: bool, data: impl Fn(&T) -> &Value) {
    items.sort_by(|left, right| {
        let left = lookup_path(data(left), path).ok().flatten();
        let right = lookup_path(data(right), path).ok().flatten();
        // Missing fields stay last regardless of direction.
        match (left, right) {
            (Some(left), Some(right)) => {
                let ordering = compare(left, right).unwrap_or(Ordering::Equal);
                if descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        Aggregate, AggregateOp, Condition, Operator, aggregate, matches, project, sort_by_path,
    };

    fn condition(path: &str, op: Operator, value: serde_json::Value) -> Condition {
        Condition {
            path: path.to_owned(),
            op,
            value,
        }
    }

    #[test]
    fn numeric_strings_compare_as_numbers_and_enums_compare_by_kind() {
        let account = json!({
            "for_votes_bp": "9000",
            "status": {"kind": "Active"},
            "author": "11111111111111111111111111111111",
            "tags": ["a", "b"]
        });
        assert!(
            matches(
                &account,
                &[condition("for_votes_bp", Operator::Gt, json!(100))]
            )
            .unwrap()
        );
        assert!(
            !matches(
                &account,
                &[condition("for_votes_bp", Operator::Lt, json!("100"))]
            )
            .unwrap()
        );
        assert!(
            matches(
                &account,
                &[condition("status", Operator::Eq, json!("Active"))]
            )
            .unwrap()
        );
        assert!(
            matches(
                &account,
                &[condition(
                    "status.kind",
                    Operator::In,
                    json!(["Draft", "Active"])
                )]
            )
            .unwrap()
        );
        assert!(
            matches(
                &account,
                &[condition("author", Operator::Contains, json!("1111"))]
            )
            .unwrap()
        );
        assert!(
            matches(
                &account,
                &[condition("tags", Operator::Contains, json!("b"))]
            )
            .unwrap()
        );
        assert!(
            matches(
                &account,
                &[condition("missing", Operator::Exists, json!(false))]
            )
            .unwrap()
        );
        assert!(!matches(&account, &[condition("missing", Operator::Eq, json!(1))]).unwrap());
    }

    #[test]
    fn projection_reports_missing_paths_as_null_and_sorting_places_missing_last() {
        let account = json!({"a": {"b": 1}, "list": [10, 20]});
        assert_eq!(
            project(&account, &["a.b".into(), "list[1]".into(), "nope".into()]).unwrap(),
            json!({"a.b": 1, "list[1]": 20, "nope": null})
        );
        let mut items = vec![
            json!({"n": "5"}),
            json!({}),
            json!({"n": 12}),
            json!({"n": "7"}),
        ];
        sort_by_path(&mut items, "n", true, |item| item);
        assert_eq!(
            items,
            vec![
                json!({"n": 12}),
                json!({"n": "7"}),
                json!({"n": "5"}),
                json!({})
            ]
        );
    }

    #[test]
    fn aggregates_keep_exact_integer_sums_beyond_f64_precision() {
        let items = [
            ("a", json!({"lamports": "9007199254740993", "stake": 5})),
            ("b", json!({"lamports": "9007199254740993", "stake": "12"})),
            ("c", json!({"lamports": null})),
        ];
        let result = aggregate(
            items.iter().map(|(key, value)| (*key, value)),
            &[
                Aggregate {
                    op: AggregateOp::Sum,
                    path: "lamports".into(),
                },
                Aggregate {
                    op: AggregateOp::Max,
                    path: "stake".into(),
                },
                Aggregate {
                    op: AggregateOp::Count,
                    path: "lamports".into(),
                },
                Aggregate {
                    op: AggregateOp::Avg,
                    path: "stake".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(result["sum(lamports)"], "18014398509481986");
        assert_eq!(result["max(stake)"], json!({"value": "12", "key": "b"}));
        assert_eq!(result["count(lamports)"], 2);
        assert_eq!(result["avg(stake)"], 8.5);
    }
}
