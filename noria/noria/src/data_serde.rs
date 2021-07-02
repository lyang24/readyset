use crate::data::DataType;
use chrono::NaiveDateTime;
use msql_srv::MysqlTime;
use serde::de::{EnumAccess, VariantAccess};
use serde::ser::SerializeTupleVariant;
use std::borrow::{Borrow, Cow};
use std::convert::TryFrom;
use std::fmt;
use std::sync::Arc;

impl serde::ser::Serialize for DataType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        match &self {
            DataType::None => serializer.serialize_unit_variant("DataType", 0, "None"),
            DataType::Int(v) => {
                serializer.serialize_newtype_variant("DataType", 1, "Int", &i128::from(*v))
            }
            DataType::UnsignedInt(v) => {
                serializer.serialize_newtype_variant("DataType", 1, "Int", &i128::from(*v))
            }
            DataType::BigInt(v) => {
                serializer.serialize_newtype_variant("DataType", 1, "Int", &i128::from(*v))
            }
            DataType::UnsignedBigInt(v) => {
                serializer.serialize_newtype_variant("DataType", 1, "Int", &i128::from(*v))
            }
            DataType::Real(f, prec) => {
                let mut tv = serializer.serialize_tuple_variant("DataType", 2, "Real", 2)?;
                tv.serialize_field(f)?;
                tv.serialize_field(prec)?;
                tv.end()
            }
            DataType::Text(v) => {
                serializer.serialize_newtype_variant("DataType", 3, "Text", v.to_bytes())
            }
            DataType::TinyText(v) => {
                let vu8 = match v.iter().position(|&i| i == 0) {
                    Some(null) => &v[0..null],
                    None => v,
                };
                serializer.serialize_newtype_variant("DataType", 3, "Text", &vu8)
            }
            DataType::Timestamp(v) => {
                serializer.serialize_newtype_variant("DataType", 4, "Timestamp", &v)
            }
            DataType::Time(v) => serializer.serialize_newtype_variant("DataType", 5, "Time", &v),
        }
    }
}

impl<'de> serde::Deserialize<'de> for DataType {
    fn deserialize<D>(deserializer: D) -> Result<DataType, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        enum Field {
            None,
            Int,
            Real,
            Text,
            Timestamp,
            Time,
        }
        struct FieldVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldVisitor {
            type Value = Field;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("variant identifier")
            }
            fn visit_u64<E>(self, val: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match val {
                    0u64 => Ok(Field::None),
                    1u64 => Ok(Field::Int),
                    2u64 => Ok(Field::Real),
                    3u64 => Ok(Field::Text),
                    4u64 => Ok(Field::Timestamp),
                    5u64 => Ok(Field::Time),
                    _ => Err(serde::de::Error::invalid_value(
                        serde::de::Unexpected::Unsigned(val),
                        &"variant index 0 <= i < 5",
                    )),
                }
            }
            fn visit_str<E>(self, val: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match val {
                    "None" => Ok(Field::None),
                    "Int" => Ok(Field::Int),
                    "Real" => Ok(Field::Real),
                    "Text" => Ok(Field::Text),
                    "Timestamp" => Ok(Field::Timestamp),
                    "Time" => Ok(Field::Time),
                    _ => Err(serde::de::Error::unknown_variant(val, VARIANTS)),
                }
            }
            fn visit_bytes<E>(self, val: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match val {
                    b"None" => Ok(Field::None),
                    b"Int" => Ok(Field::Int),
                    b"Real" => Ok(Field::Real),
                    b"Text" => Ok(Field::Text),
                    b"Timestamp" => Ok(Field::Timestamp),
                    b"Time" => Ok(Field::Time),
                    _ => Err(serde::de::Error::unknown_variant(
                        &String::from_utf8_lossy(val),
                        VARIANTS,
                    )),
                }
            }
        }
        impl<'de> serde::Deserialize<'de> for Field {
            #[inline]
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                serde::Deserializer::deserialize_identifier(deserializer, FieldVisitor)
            }
        }

        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = DataType;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("enum DataType")
            }

            fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
            where
                A: EnumAccess<'de>,
            {
                match EnumAccess::variant(data)? {
                    (Field::None, variant) => {
                        match VariantAccess::unit_variant(variant) {
                            Ok(val) => val,
                            Err(err) => {
                                return Err(err);
                            }
                        };
                        Ok(DataType::None)
                    }
                    (Field::Int, variant) => VariantAccess::newtype_variant::<i128>(variant)
                        .and_then(|x| {
                            DataType::try_from(x).map_err(|_| {
                                serde::de::Error::invalid_value(
                                    serde::de::Unexpected::Other(format!("{}", x).as_str()),
                                    &"integer (i128)",
                                )
                            })
                        }),
                    (Field::Real, variant) => {
                        struct Visitor;
                        impl<'de> serde::de::Visitor<'de> for Visitor {
                            type Value = DataType;
                            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                                fmt::Formatter::write_str(formatter, "tuple variant DataType::Real")
                            }
                            #[inline]
                            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                            where
                                A: serde::de::SeqAccess<'de>,
                            {
                                let f = seq.next_element()?.ok_or_else(|| {
                                    serde::de::Error::invalid_length(
                                        0usize,
                                        &"tuple variant DataType::Real with 2 elements",
                                    )
                                })?;
                                let prec = seq.next_element()?.ok_or_else(|| {
                                    serde::de::Error::invalid_length(
                                        0usize,
                                        &"tuple variant DataType::Real with 2 elements",
                                    )
                                })?;
                                Ok(DataType::Real(f, prec))
                            }
                        }
                        VariantAccess::tuple_variant(variant, 4usize, Visitor)
                    }
                    (Field::Text, variant) => {
                        VariantAccess::newtype_variant::<Cow<'_, [u8]>>(variant).and_then(|x| {
                            let x: &[u8] = x.borrow();
                            DataType::try_from(x).map_err(|_| {
                                serde::de::Error::invalid_value(
                                    serde::de::Unexpected::Bytes(x),
                                    &"valid utf-8 or short TinyText",
                                )
                            })
                        })
                    }
                    (Field::Timestamp, variant) => Result::map(
                        VariantAccess::newtype_variant::<NaiveDateTime>(variant),
                        DataType::Timestamp,
                    ),
                    (Field::Time, variant) => VariantAccess::newtype_variant::<MysqlTime>(variant)
                        .map(Arc::new)
                        .map(DataType::Time),
                }
            }
        }

        const VARIANTS: &[&str] = &[
            "None",
            "Int",
            "UnsignedInt",
            "BigInt",
            "UnsignedBigInt",
            "Real",
            "Text",
            "Timestamp",
            "Time",
        ];
        deserializer.deserialize_enum("DataType", VARIANTS, Visitor)
    }
}
