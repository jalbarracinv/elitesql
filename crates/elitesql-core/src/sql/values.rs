//! SQL values implementation shared by execution and its tests.
use super::*;

// --- literals ------------------------------------------------------------------

/// Schema-aware coercion for INSERT/UPDATE SET values.
pub(super) fn literal_to_value(lit: &Literal, ty: ColumnType, col: &str) -> Result<Value> {
    if let Literal::Bound(value) = lit {
        return bound_value_for_column(value, ty, col);
    }
    let out = match (lit, ty) {
        (Literal::Null, _) => Value::Null,
        (Literal::Bool(b), ColumnType::Bool) => Value::Bool(*b),
        (Literal::Int(n), ColumnType::Int64) => Value::Int64(*n),
        (Literal::Int(n), ColumnType::Float64) => Value::Float64(*n as f64),
        (Literal::Int(n), ColumnType::Timestamp) => Value::Timestamp(*n),
        (Literal::Float(f), ColumnType::Float64) => Value::Float64(*f),
        (Literal::Str(s), ColumnType::Text) => Value::Text(s.clone()),
        (Literal::Str(s), ColumnType::Json) => {
            let j = serde_json::from_str(s)
                .map_err(|e| Error::Sql(format!("invalid json literal for '{col}': {e}")))?;
            Value::Json(j)
        }
        (Literal::Str(s), ColumnType::Vector) => {
            let v: Vec<f32> = serde_json::from_str(s).map_err(|e| {
                Error::Sql(format!(
                    "invalid vector literal for '{col}' (expected a JSON array of numbers): {e}"
                ))
            })?;
            Value::Vector(v)
        }
        (Literal::Str(s), ColumnType::Timestamp) => Value::parse_timestamp(s).ok_or_else(|| {
            Error::Sql(format!(
                "invalid timestamp literal for '{col}': expected 'YYYY-MM-DD HH:MM:SS[.ffffff]' (UTC)"
            ))
        })?,
        (Literal::Str(s), ColumnType::Date) => Value::parse_date(s).ok_or_else(|| {
            Error::Sql(format!("invalid date literal for '{col}': expected 'YYYY-MM-DD' with a real date"))
        })?,
        (Literal::Str(s), ColumnType::Time) => Value::parse_time(s).ok_or_else(|| {
            Error::Sql(format!("invalid time literal for '{col}': expected 'HH:MM:SS[.ffffff]'"))
        })?,
        (Literal::Int(n), ColumnType::Date) => {
            let d = i32::try_from(*n).map_err(|_| {
                Error::Sql(format!("date literal out of range for '{col}'"))
            })?;
            Value::Date(d)
        }
        (Literal::Int(n), ColumnType::Time) => {
            if !(0..86_400_000_000).contains(n) {
                return Err(Error::Sql(format!(
                    "time literal out of range for '{col}': 0..86400000000 microseconds"
                )));
            }
            Value::Time(*n)
        }
        (Literal::Blob(b), ColumnType::Blob) => Value::Blob(b.clone()),
        (
            Literal::PositionalParam
            | Literal::NamedParam(_)
            | Literal::Bound(_)
            | Literal::CurrentTimestamp,
            _,
        ) => {
            unreachable!("parameters are bound before execution")
        }
        _ => {
            return Err(Error::Sql(format!(
                "literal {lit:?} is not valid for column '{col}' of type {ty}"
            )))
        }
    };
    Ok(out)
}

pub(super) fn bound_value_for_column(value: &Value, ty: ColumnType, col: &str) -> Result<Value> {
    if value.is_null() || value.matches(ty) {
        return Ok(value.clone());
    }
    let converted = match (value, ty) {
        (Value::Int64(value), ColumnType::Float64) => Some(Value::Float64(*value as f64)),
        (Value::Int64(value), ColumnType::Timestamp) => Some(Value::Timestamp(*value)),
        (Value::Int64(value), ColumnType::Date) => {
            Some(Value::Date(i32::try_from(*value).map_err(|_| {
                Error::Sql(format!("date parameter out of range for '{col}'"))
            })?))
        }
        (Value::Int64(value), ColumnType::Time) if (0..86_400_000_000).contains(value) => {
            Some(Value::Time(*value))
        }
        (Value::Text(value), ColumnType::Timestamp) => Value::parse_timestamp(value),
        (Value::Text(value), ColumnType::Date) => Value::parse_date(value),
        (Value::Text(value), ColumnType::Time) => Value::parse_time(value),
        (Value::Text(value), ColumnType::Json) => {
            Some(Value::Json(serde_json::Value::String(value.clone())))
        }
        (Value::Bool(value), ColumnType::Json) => Some(Value::Json((*value).into())),
        (Value::Int64(value), ColumnType::Json) => Some(Value::Json((*value).into())),
        (Value::Float64(value), ColumnType::Json) => {
            serde_json::Number::from_f64(*value).map(|number| Value::Json(number.into()))
        }
        (Value::Json(serde_json::Value::Array(values)), ColumnType::Vector) => {
            let mut vector = Vec::with_capacity(values.len());
            for component in values {
                let Some(component) = component.as_f64() else {
                    return Err(Error::Sql(format!(
                        "vector parameter for '{col}' contains a non-numeric component"
                    )));
                };
                if !component.is_finite()
                    || component < f32::MIN as f64
                    || component > f32::MAX as f64
                {
                    return Err(Error::Sql(format!(
                        "vector parameter for '{col}' contains a component outside finite float32"
                    )));
                }
                vector.push(component as f32);
            }
            Some(Value::Vector(vector))
        }
        _ => None,
    };
    converted.ok_or_else(|| {
        Error::Sql(format!(
            "parameter value {value:?} is not valid for column '{col}' of type {ty}"
        ))
    })
}

/// Context-free conversion for WHERE comparisons.
pub(super) fn literal_to_plain_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Int(n) => Value::Int64(*n),
        Literal::Float(f) => Value::Float64(*f),
        Literal::Str(s) => Value::Text(s.clone()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Bound(value) => value.clone(),
        Literal::PositionalParam | Literal::NamedParam(_) | Literal::CurrentTimestamp => {
            unreachable!("parameters are bound before expression resolution")
        }
    }
}

// --- value comparison ------------------------------------------------------------

pub(super) fn compare_integer_float(integer: i64, float: f64) -> Ordering {
    if float.is_nan() {
        return (integer as f64).total_cmp(&float);
    }
    if float < i64::MIN as f64 {
        return Ordering::Greater;
    }
    // i64::MAX rounds up to 2^63 as f64. Test the exclusive bound before
    // casting so saturation cannot turn MAX and 2^63 into equal values.
    if float >= -(i64::MIN as f64) {
        return Ordering::Less;
    }
    match integer.cmp(&(float as i64)) {
        Ordering::Equal if float == 0.0 => 0.0f64.total_cmp(&float),
        Ordering::Equal => 0.0f64.partial_cmp(&float.fract()).expect("finite float"),
        order => order,
    }
}

pub(super) fn exact_float_integer(float: f64) -> Option<i64> {
    let integer = float as i64;
    (float.is_finite() && compare_integer_float(integer, float) == Ordering::Equal)
        .then_some(integer)
}

pub(super) fn compare_numeric(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int64(a) | Value::Timestamp(a), Value::Int64(b) | Value::Timestamp(b)) => {
            Some(a.cmp(b))
        }
        (Value::Int64(a) | Value::Timestamp(a), Value::Float64(b)) => {
            Some(compare_integer_float(*a, *b))
        }
        (Value::Float64(a), Value::Int64(b) | Value::Timestamp(b)) => {
            Some(compare_integer_float(*b, *a).reverse())
        }
        (Value::Float64(a), Value::Float64(b)) => Some(a.total_cmp(b)),
        _ => None,
    }
}

/// Strict ordering for WHERE comparisons; None = not comparable.
/// Text literals coerce against date/time columns ('2026-08-07', '09:30:00')
/// so natural predicates work without a cast syntax.
pub(super) fn cmp_vals(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Blob(x), Value::Blob(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::Time(x), Value::Time(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Text(s)) => match Value::parse_date(s) {
            Some(Value::Date(y)) => Some(x.cmp(&y)),
            _ => None,
        },
        (Value::Text(s), Value::Date(y)) => match Value::parse_date(s) {
            Some(Value::Date(x)) => Some(x.cmp(y)),
            _ => None,
        },
        (Value::Time(x), Value::Text(s)) => match Value::parse_time(s) {
            Some(Value::Time(y)) => Some(x.cmp(&y)),
            _ => None,
        },
        (Value::Text(s), Value::Time(y)) => match Value::parse_time(s) {
            Some(Value::Time(x)) => Some(x.cmp(y)),
            _ => None,
        },
        (Value::Timestamp(x), Value::Text(s)) => match Value::parse_timestamp(s) {
            Some(Value::Timestamp(y)) => Some(x.cmp(&y)),
            _ => None,
        },
        (Value::Text(s), Value::Timestamp(y)) => match Value::parse_timestamp(s) {
            Some(Value::Timestamp(x)) => Some(x.cmp(y)),
            _ => None,
        },
        _ => compare_numeric(a, b),
    }
}

pub(super) fn eq_vals(a: &Value, b: &Value) -> Option<bool> {
    if let (Value::Json(x), Value::Json(y)) = (a, b) {
        return Some(x == y);
    }
    cmp_vals(a, b).map(|o| o == Ordering::Equal)
}

/// Total order for ORDER BY: never fails, NULLs first, then by type family.
pub(super) fn sort_cmp(a: &Value, b: &Value, collation: Collation) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int64(_) | Value::Float64(_) | Value::Timestamp(_) => 2,
            Value::Text(_) => 3,
            Value::Blob(_) => 4,
            Value::Json(_) => 5,
            Value::Vector(_) => 6,
            Value::Date(_) => 7,
            Value::Time(_) => 8,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => collation.compare(x, y),
        (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        (Value::Json(x), Value::Json(y)) => x.to_string().cmp(&y.to_string()),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Time(x), Value::Time(y)) => x.cmp(y),
        _ => compare_numeric(a, b).unwrap_or(Ordering::Equal),
    }
}
