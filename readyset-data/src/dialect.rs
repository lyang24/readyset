use serde::{Deserialize, Serialize};

use crate::DfType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SqlEngine {
    PostgreSQL,
    MySQL,
}

/// Information for how the semantics of a query should be evaluated, in places where those
/// semantics differ between database implementations, or are configurable at runtime in any way.
///
/// A note on the structure and naming of this type: Currently, this type is functionally identical
/// to [`nom_sql::Dialect`], but that's mostly indicental - in the future, this type will be
/// expanded to include things like the MySQL `SQL_MODE` flag, etc. As a rule, [`nom_sql::Dialect`]
/// is for configuring *parsing*, whereas this type is for configuring *semantics of evaluation*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dialect {
    engine: SqlEngine,
}

impl Dialect {
    /// The [`Dialect`] corresponding to the expression evaluation semantics of a
    /// default-configured PostgreSQL database.
    pub const DEFAULT_POSTGRESQL: Dialect = Dialect {
        engine: SqlEngine::PostgreSQL,
    };

    /// The [`Dialect`] corresponding to the expression evaluation semantics of a
    /// default-configured MySQL database.
    pub const DEFAULT_MYSQL: Dialect = Dialect {
        engine: SqlEngine::MySQL,
    };

    /// Return an enum corresponding to the underlying SQL engine for this dialect.
    ///
    /// This function should ideally be used quite sparingly, instead opting to encode
    /// engine-specific behavior via methods on this type.
    pub fn engine(&self) -> SqlEngine {
        self.engine
    }

    /// Returns the default subsecond digit count for time types.
    ///
    /// This value is also known as fractional second precision (FSP), and can be queried via
    /// `datetime_precision` in [`information_schema.columns`](https://dev.mysql.com/doc/refman/8.0/en/information-schema-columns-table.html).
    #[inline]
    pub fn default_subsecond_digits(self) -> u16 {
        match self.engine {
            // https://dev.mysql.com/doc/refman/8.0/en/date-and-time-type-syntax.html
            // "If omitted, the default precision is 0. (This differs from the standard SQL default
            // of 6, for compatibility with previous MySQL versions.)"
            SqlEngine::MySQL => 0,

            // https://www.postgresql.org/docs/current/datatype-datetime.html
            // "By default, there is no explicit bound on precision", so the max value is used.
            SqlEngine::PostgreSQL => 6,
        }
    }

    /// Returns whether to treat the `||` operator as a concatenation operator.
    ///
    /// This returns true for Postgres, and also should return true for MySQL if `PIPES_AS_CONCAT`
    /// is set in the SQL mode, but currently that is not yet implemented. (The default for MySQL
    /// is to treat `||` as a boolean OR, so it should be okay that we just return `false` for
    /// MySQL right now.)
    pub fn double_pipe_is_concat(self) -> bool {
        match self.engine {
            SqlEngine::MySQL => false,
            SqlEngine::PostgreSQL => true,
        }
    }

    /// Return the [`DfType`] corresponding to the SQL `FLOAT` type for this dialect
    pub(crate) fn float_type(&self) -> DfType {
        match self.engine {
            SqlEngine::MySQL => DfType::Float,
            SqlEngine::PostgreSQL => DfType::Double,
        }
    }

    /// Return the [`DfType`] corresponding to the SQL `REAL` type for this dialect
    pub(crate) fn real_type(&self) -> DfType {
        // TODO: Handle `real_as_float` mode.
        match self.engine {
            SqlEngine::PostgreSQL => DfType::Double,
            SqlEngine::MySQL => DfType::Float,
        }
    }

    /// Return the [`DfType`] corresponding to the SQL `Serial` type for this dialect
    pub(crate) fn serial_type(&self) -> DfType {
        match self.engine {
            SqlEngine::MySQL => DfType::UnsignedBigInt,
            SqlEngine::PostgreSQL => DfType::Int,
        }
    }
}
