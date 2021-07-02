use std::cmp::Ordering;
use std::fmt;
use std::str;
use std::str::FromStr;

use crate::keywords::escape_if_keyword;
use crate::FunctionExpression;
use crate::{
    common::{
        column_identifier_no_alias, parse_comment, sql_identifier, type_identifier, Literal,
        SqlType,
    },
    Real,
};
use nom::bytes::complete::{tag_no_case, take_until};
use nom::character::complete::{multispace0, multispace1};
use nom::combinator::{map, opt};
use nom::multi::many0;
use nom::sequence::{delimited, preceded, terminated, tuple};
use nom::{alt, complete, do_parse, named, opt, tag, tag_no_case, IResult};
use nom::{branch::alt, bytes::complete::tag, character::complete::digit1};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub table: Option<String>,
    pub function: Option<Box<FunctionExpression>>,
}

impl fmt::Display for Column {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(ref table) = self.table {
            write!(
                f,
                "{}.{}",
                escape_if_keyword(table),
                escape_if_keyword(&self.name)
            )?;
        } else if let Some(ref function) = self.function {
            write!(f, "{}", *function)?;
        } else {
            write!(f, "{}", escape_if_keyword(&self.name))?;
        }
        Ok(())
    }
}

impl<'a> From<&'a str> for Column {
    fn from(c: &str) -> Column {
        match c.find('.') {
            None => Column {
                name: String::from(c),
                table: None,
                function: None,
            },
            Some(i) => Column {
                name: String::from(&c[i + 1..]),
                table: Some(String::from(&c[0..i])),
                function: None,
            },
        }
    }
}

impl Ord for Column {
    fn cmp(&self, other: &Column) -> Ordering {
        if self.table.is_some() && other.table.is_some() {
            match self.table.cmp(&other.table) {
                Ordering::Equal => self.name.cmp(&other.name),
                x => x,
            }
        } else {
            self.name.cmp(&other.name)
        }
    }
}

impl PartialOrd for Column {
    fn partial_cmp(&self, other: &Column) -> Option<Ordering> {
        if self.table.is_some() && other.table.is_some() {
            match self.table.cmp(&other.table) {
                Ordering::Equal => Some(self.name.cmp(&other.name)),
                x => Some(x),
            }
        } else if self.table.is_none() && other.table.is_none() {
            Some(self.name.cmp(&other.name))
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ColumnConstraint {
    Null,
    NotNull,
    CharacterSet(String),
    Collation(String),
    DefaultValue(Literal),
    AutoIncrement,
    PrimaryKey,
    Unique,
    /// NOTE(grfn): Yes, this really is its own special thing, not just an expression - see
    /// <https://dev.mysql.com/doc/refman/8.0/en/timestamp-initialization.html>
    OnUpdateCurrentTimestamp,
}

impl fmt::Display for ColumnConstraint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ColumnConstraint::Null => write!(f, "NULL"),
            ColumnConstraint::NotNull => write!(f, "NOT NULL"),
            ColumnConstraint::CharacterSet(ref charset) => write!(f, "CHARACTER SET {}", charset),
            ColumnConstraint::Collation(ref collation) => write!(f, "COLLATE {}", collation),
            ColumnConstraint::DefaultValue(ref literal) => {
                write!(f, "DEFAULT {}", literal.to_string())
            }
            ColumnConstraint::AutoIncrement => write!(f, "AUTO_INCREMENT"),
            ColumnConstraint::PrimaryKey => write!(f, "PRIMARY KEY"),
            ColumnConstraint::Unique => write!(f, "UNIQUE"),
            ColumnConstraint::OnUpdateCurrentTimestamp => write!(f, "ON UPDATE CURRENT_TIMESTAMP"),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ColumnSpecification {
    pub column: Column,
    pub sql_type: SqlType,
    pub constraints: Vec<ColumnConstraint>,
    pub comment: Option<String>,
}

impl fmt::Display for ColumnSpecification {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} {}",
            escape_if_keyword(&self.column.name),
            self.sql_type
        )?;
        for constraint in self
            .constraints
            .iter()
            // Don't output PRIMARY KEY, because it will be formatted as table level key instead
            .filter(|c| !matches!(c, ColumnConstraint::PrimaryKey))
        {
            write!(f, " {}", constraint)?;
        }
        if let Some(ref comment) = self.comment {
            write!(f, " COMMENT '{}'", comment)?;
        }
        Ok(())
    }
}

impl ColumnSpecification {
    pub fn new(column: Column, sql_type: SqlType) -> ColumnSpecification {
        ColumnSpecification {
            column,
            sql_type,
            constraints: vec![],
            comment: None,
        }
    }

    pub fn with_constraints(
        column: Column,
        sql_type: SqlType,
        constraints: Vec<ColumnConstraint>,
    ) -> ColumnSpecification {
        ColumnSpecification {
            column,
            sql_type,
            constraints,
            comment: None,
        }
    }

    pub fn convert_column(&self) -> msql_srv::Column {
        let mut colflags = msql_srv::ColumnFlags::empty();
        use msql_srv::ColumnType::*;

        let coltype = match self.sql_type {
            SqlType::Mediumtext => MYSQL_TYPE_VAR_STRING,
            SqlType::Longtext => MYSQL_TYPE_BLOB,
            SqlType::Text => MYSQL_TYPE_STRING,
            SqlType::Varchar(_) => MYSQL_TYPE_VAR_STRING,
            SqlType::Int(_) => MYSQL_TYPE_LONG,
            SqlType::UnsignedInt(_) => {
                colflags |= msql_srv::ColumnFlags::UNSIGNED_FLAG;
                MYSQL_TYPE_LONG
            }
            SqlType::Bigint(_) => MYSQL_TYPE_LONGLONG,
            SqlType::UnsignedBigint(_) => {
                colflags |= msql_srv::ColumnFlags::UNSIGNED_FLAG;
                MYSQL_TYPE_LONGLONG
            }
            SqlType::Tinyint(_) => MYSQL_TYPE_TINY,
            SqlType::UnsignedTinyint(_) => {
                colflags |= msql_srv::ColumnFlags::UNSIGNED_FLAG;
                MYSQL_TYPE_TINY
            }
            SqlType::Smallint(_) => MYSQL_TYPE_SHORT,
            SqlType::UnsignedSmallint(_) => {
                colflags |= msql_srv::ColumnFlags::UNSIGNED_FLAG;
                MYSQL_TYPE_SHORT
            }
            SqlType::Bool => MYSQL_TYPE_BIT,
            SqlType::DateTime(_) => MYSQL_TYPE_DATETIME,
            SqlType::Float => MYSQL_TYPE_DOUBLE,
            SqlType::Decimal(_, _) => MYSQL_TYPE_DECIMAL,
            SqlType::Char(_) => {
                // TODO(grfn): I'm not sure if this is right
                MYSQL_TYPE_STRING
            }
            SqlType::Blob => MYSQL_TYPE_BLOB,
            SqlType::Longblob => MYSQL_TYPE_LONG_BLOB,
            SqlType::Mediumblob => MYSQL_TYPE_MEDIUM_BLOB,
            SqlType::Tinyblob => MYSQL_TYPE_TINY_BLOB,
            SqlType::Double => MYSQL_TYPE_DOUBLE,
            SqlType::Real => {
                // a generous reading of
                // https://dev.mysql.com/doc/refman/8.0/en/floating-point-types.html seems to
                // indicate that real is equivalent to float
                // TODO(grfn): Make sure that's the case
                MYSQL_TYPE_FLOAT
            }
            SqlType::Tinytext => {
                // TODO(grfn): How does the mysql binary protocol handle
                // tinytext? is it just an alias for tinyblob or is there a flag
                // we need?
                unimplemented!()
            }
            SqlType::Date => MYSQL_TYPE_DATE,
            SqlType::Timestamp => MYSQL_TYPE_TIMESTAMP,
            SqlType::Binary(_) => {
                // TODO(grfn): I don't know if this is right
                colflags |= msql_srv::ColumnFlags::BINARY_FLAG;
                MYSQL_TYPE_STRING
            }
            SqlType::Varbinary(_) => {
                // TODO(grfn): I don't know if this is right
                colflags |= msql_srv::ColumnFlags::BINARY_FLAG;
                MYSQL_TYPE_VAR_STRING
            }
            SqlType::Enum(_) => {
                // TODO(grfn): I don't know if this is right
                colflags |= msql_srv::ColumnFlags::ENUM_FLAG;
                MYSQL_TYPE_VAR_STRING
            }
            SqlType::Time => MYSQL_TYPE_TIME,
        };

        for c in &self.constraints {
            match *c {
                ColumnConstraint::AutoIncrement => {
                    colflags |= msql_srv::ColumnFlags::AUTO_INCREMENT_FLAG;
                }
                ColumnConstraint::NotNull => {
                    colflags |= msql_srv::ColumnFlags::NOT_NULL_FLAG;
                }
                ColumnConstraint::PrimaryKey => {
                    colflags |= msql_srv::ColumnFlags::PRI_KEY_FLAG;
                }
                ColumnConstraint::Unique => {
                    colflags |= msql_srv::ColumnFlags::UNIQUE_KEY_FLAG;
                }
                _ => (),
            }
        }

        msql_srv::Column {
            table: self.column.table.clone().unwrap_or_default(),
            column: self.column.name.clone(),
            coltype,
            colflags,
        }
    }
}

fn fixed_point(i: &[u8]) -> IResult<&[u8], Literal> {
    let (remaining_input, (i, _, f)) = tuple((digit1, tag("."), digit1))(i)?;
    let precision = f.len();
    let int = i32::from_str(str::from_utf8(i).unwrap()).unwrap();
    let dec = i32::from_str(str::from_utf8(f).unwrap()).unwrap();
    let value = (int as f64) + (dec as f64) / 10.0_f64.powf(precision as f64);
    Ok((
        remaining_input,
        Literal::FixedPoint(Real {
            value,
            precision: precision as u8,
        }),
    ))
}

fn default(i: &[u8]) -> IResult<&[u8], ColumnConstraint> {
    let (remaining_input, (_, _, _, def, _)) = tuple((
        multispace0,
        tag_no_case("default"),
        multispace1,
        // TODO(grfn): This really should just be a generic expression parser T.T
        // https://app.clubhouse.io/readysettech/story/101/unify-the-expression-ast
        alt((
            map(
                delimited(tag("'"), take_until("'"), tag("'")),
                |s: &[u8]| Literal::String(String::from_utf8(s.to_vec()).unwrap()),
            ),
            fixed_point,
            map(digit1, |d| {
                let d_i64 = i64::from_str(str::from_utf8(d).unwrap()).unwrap();
                Literal::Integer(d_i64)
            }),
            map(tag("''"), |_| Literal::String(String::from(""))),
            map(tag_no_case("null"), |_| Literal::Null),
            map(
                terminated(tag_no_case("current_timestamp"), opt(tag("()"))),
                |_| Literal::CurrentTimestamp,
            ),
        )),
        multispace0,
    ))(i)?;

    Ok((remaining_input, ColumnConstraint::DefaultValue(def)))
}

named!(
    on_update_current_timestamp(&[u8]) -> ColumnConstraint,
    do_parse!(
        complete!(tag_no_case!("on"))
            >> multispace1
            >> complete!(tag_no_case!("update"))
            >> multispace1
            >> alt!(
                tag_no_case!("current_timestamp")
                    | tag_no_case!("now")
                    | tag_no_case!("localtime")
                    | tag_no_case!("localtimestamp")
            )
            >> opt!(tag!("()"))
            >> (ColumnConstraint::OnUpdateCurrentTimestamp)
    )
);

pub fn column_constraint(i: &[u8]) -> IResult<&[u8], ColumnConstraint> {
    let not_null = map(
        delimited(multispace0, tag_no_case("not null"), multispace0),
        |_| ColumnConstraint::NotNull,
    );
    let null = map(
        delimited(multispace0, tag_no_case("null"), multispace0),
        |_| ColumnConstraint::Null,
    );
    let auto_increment = map(
        delimited(multispace0, tag_no_case("auto_increment"), multispace0),
        |_| ColumnConstraint::AutoIncrement,
    );
    let primary_key = map(
        delimited(multispace0, tag_no_case("primary key"), multispace0),
        |_| ColumnConstraint::PrimaryKey,
    );
    let unique = map(
        delimited(
            multispace0,
            delimited(tag_no_case("unique"), multispace0, opt(tag_no_case("key"))),
            multispace0,
        ),
        |_| ColumnConstraint::Unique,
    );
    let character_set = map(
        preceded(
            delimited(multispace0, tag_no_case("character set"), multispace1),
            sql_identifier,
        ),
        |cs| {
            let char_set = str::from_utf8(cs).unwrap().to_owned();
            ColumnConstraint::CharacterSet(char_set)
        },
    );
    let collate = map(
        preceded(
            delimited(multispace0, tag_no_case("collate"), multispace1),
            sql_identifier,
        ),
        |c| {
            let collation = str::from_utf8(c).unwrap().to_owned();
            ColumnConstraint::Collation(collation)
        },
    );

    alt((
        not_null,
        null,
        auto_increment,
        default,
        primary_key,
        unique,
        character_set,
        collate,
        on_update_current_timestamp,
    ))(i)
}

/// Parse rule for a column specification
pub fn column_specification(i: &[u8]) -> IResult<&[u8], ColumnSpecification> {
    let (remaining_input, (column, field_type, constraints, comment)) = tuple((
        column_identifier_no_alias,
        opt(delimited(multispace1, type_identifier, multispace0)),
        many0(column_constraint),
        opt(parse_comment),
    ))(i)?;

    let sql_type = match field_type {
        None => SqlType::Text,
        Some(ref t) => t.clone(),
    };
    Ok((
        remaining_input,
        ColumnSpecification {
            column,
            sql_type,
            constraints,
            comment,
        },
    ))
}

#[cfg(not(feature = "postgres"))]
#[cfg(test)]
mod tests_mysql {
    use super::*;

    #[test]
    fn multiple_constraints() {
        let (_, res) =
            column_specification(b"`created_at` timestamp NOT NULL DEFAULT current_timestamp()")
                .unwrap();
        assert_eq!(
            res,
            ColumnSpecification {
                column: Column {
                    name: "created_at".to_owned(),
                    table: None,
                    function: None
                },
                sql_type: SqlType::Timestamp,
                comment: None,
                constraints: vec![
                    ColumnConstraint::NotNull,
                    ColumnConstraint::DefaultValue(Literal::CurrentTimestamp),
                ]
            }
        );
    }

    #[test]
    fn null_round_trip() {
        let input = b"c INT(32) NULL";
        let cspec = column_specification(input).unwrap().1;
        let res = cspec.to_string();
        assert_eq!(res, String::from_utf8(input.to_vec()).unwrap());
    }
}

#[cfg(feature = "postgres")]
#[cfg(test)]
mod tests_postgres {
    use super::*;

    #[test]
    fn multiple_constraints() {
        let (_, res) =
            column_specification(b"\"created_at\" timestamp NOT NULL DEFAULT current_timestamp()")
                .unwrap();
        assert_eq!(
            res,
            ColumnSpecification {
                column: Column {
                    name: "created_at".to_owned(),
                    table: None,
                    function: None
                },
                sql_type: SqlType::Timestamp,
                comment: None,
                constraints: vec![
                    ColumnConstraint::NotNull,
                    ColumnConstraint::DefaultValue(Literal::CurrentTimestamp),
                ]
            }
        );
    }
}
