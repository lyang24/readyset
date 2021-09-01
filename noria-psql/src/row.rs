use crate::value::Value;
use noria::DataType;
use std::sync::Arc;
use tokio_postgres::types::Type;

/// A structure containing a `Vec<DataType>`, representing one row of data, which facilitates
/// iteration over the values within this row as `Value` structures.
pub struct Row {
    /// The values comprising the row, as returned from a Noria interface lookup. Only the indices
    /// within this vector listed in `project_fields` will actually be projected during iteration.
    /// (See documentaion below for `project_fields`).
    pub values: Vec<DataType>,

    /// The fields to project. A `Vec<DataType>` returned from a Noria interface lookup may
    /// contain extraneous fields that should not be projected into the query result output. In
    /// particular, bogokeys and other lookup keys that are not requested for projection by the SQL
    /// query may be present in `values` but should be excluded from query output. This
    /// `project_fields` attribute contains the indices within `values` that _should_ be projected
    /// into the output.
    pub project_fields: Arc<Vec<usize>>,

    /// The data types of the projected fields for this row.
    pub project_field_types: Arc<Vec<Type>>,
}

impl IntoIterator for Row {
    type Item = Value;
    type IntoIter = RowIterator;

    fn into_iter(self) -> Self::IntoIter {
        RowIterator { row: self, pos: 0 }
    }
}

/// An iterator over a `Row`'s values. Only those field values listed in the `Row`'s
/// `project_fields` list are included in the iterator output.
pub struct RowIterator {
    /// The row being iterated.
    row: Row,

    /// The iteration position.
    pos: usize,
}

impl Iterator for RowIterator {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        let col_type = self.row.project_field_types.get(self.pos)?.clone();
        let i = *self.row.project_fields.get(self.pos)?;
        let value = self.row.values.get(i)?.clone();
        self.pos += 1;
        Some(Value { col_type, value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arccstr::ArcCStr;
    use psql_srv as ps;
    use std::convert::TryFrom;

    fn collect_row_values(row: Row) -> Vec<ps::Value> {
        row.into_iter()
            .map(|v| ps::Value::try_from(v).unwrap())
            .collect::<Vec<ps::Value>>()
    }

    #[test]
    fn iterate_empty_row() {
        let row = Row {
            values: vec![],
            project_fields: Arc::new(vec![]),
            project_field_types: Arc::new(vec![]),
        };
        assert_eq!(collect_row_values(row), Vec::<ps::Value>::new());
    }

    #[test]
    fn iterate_singleton_row() {
        let row = Row {
            values: vec![DataType::Int(43)],
            project_fields: Arc::new(vec![0]),
            project_field_types: Arc::new(vec![Type::INT4]),
        };
        assert_eq!(collect_row_values(row), vec![ps::Value::Int(43)]);
    }

    #[test]
    fn iterate_row() {
        let row = Row {
            values: vec![
                DataType::Int(43),
                DataType::Text(ArcCStr::try_from("abcde").unwrap()),
                DataType::Double(10.000000222, 9),
                DataType::Float(8.99, 2),
            ],
            project_fields: Arc::new(vec![0, 1, 2, 3]),
            project_field_types: Arc::new(vec![Type::INT4, Type::TEXT, Type::FLOAT8, Type::FLOAT4]),
        };
        assert_eq!(
            collect_row_values(row),
            vec![
                ps::Value::Int(43),
                ps::Value::Text(ArcCStr::try_from("abcde").unwrap()),
                ps::Value::Double(10.000000222),
                ps::Value::Real(8.99)
            ]
        );
    }

    #[test]
    fn iterate_row_with_trailing_unprojected_fields() {
        let row = Row {
            values: vec![
                DataType::Int(43),
                DataType::Text(ArcCStr::try_from("abcde").unwrap()),
                DataType::Double(10.000000222, 9),
                DataType::Float(8.99, 2),
                DataType::Int(0),
            ],
            // Only the first three fields are specified for projection.
            project_fields: Arc::new(vec![0, 1, 2, 3]),
            project_field_types: Arc::new(vec![Type::INT4, Type::TEXT, Type::FLOAT8, Type::FLOAT4]),
        };
        assert_eq!(
            collect_row_values(row),
            vec![
                ps::Value::Int(43),
                ps::Value::Text(ArcCStr::try_from("abcde").unwrap()),
                ps::Value::Double(10.000000222),
                ps::Value::Real(8.99)
            ]
        );
    }

    #[test]
    fn iterate_row_with_interleaved_unprojected_fields() {
        let row = Row {
            values: vec![
                DataType::Int(0),
                DataType::Int(43),
                DataType::Text(ArcCStr::try_from("abcde").unwrap()),
                DataType::Int(0),
                DataType::Int(0),
                DataType::Double(10.000000222, 9),
                DataType::Float(8.99, 2),
                DataType::Int(0),
            ],
            // Only some of the fields are specified for projection.
            project_fields: Arc::new(vec![1, 2, 5, 6]),
            project_field_types: Arc::new(vec![Type::INT4, Type::TEXT, Type::FLOAT8, Type::FLOAT4]),
        };
        assert_eq!(
            collect_row_values(row),
            vec![
                ps::Value::Int(43),
                ps::Value::Text(ArcCStr::try_from("abcde").unwrap()),
                ps::Value::Double(10.000000222),
                ps::Value::Real(8.99)
            ]
        );
    }
}
