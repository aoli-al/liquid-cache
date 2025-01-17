#![allow(unused)]

use std::collections::VecDeque;
use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef};
use parquet::arrow::array_reader::ArrayReader;
use parquet::arrow::arrow_reader::{RowSelection, RowSelector};
use parquet::schema::types::TypePtr;

/// Representation of a parquet schema element, in terms of arrow schema elements
#[derive(Debug, Clone)]
pub struct ParquetField {
    /// The level which represents an insertion into the current list
    /// i.e. guaranteed to be > 0 for an element of list type
    pub rep_level: i16,
    /// The level at which this field is fully defined,
    /// i.e. guaranteed to be > 0 for a nullable type or child of a
    /// nullable type
    pub def_level: i16,
    /// Whether this field is nullable
    pub nullable: bool,
    /// The arrow type of the column data
    ///
    /// Note: In certain cases the data stored in parquet may have been coerced
    /// to a different type and will require conversion on read (e.g. Date64 and Interval)
    pub arrow_type: DataType,
    /// The type of this field
    pub field_type: ParquetFieldType,
}

impl ParquetField {
    /// Converts `self` into an arrow list, with its current type as the field type
    ///
    /// This is used to convert repeated columns, into their arrow representation
    fn into_list(self, name: &str) -> Self {
        ParquetField {
            rep_level: self.rep_level,
            def_level: self.def_level,
            nullable: false,
            arrow_type: DataType::List(Arc::new(Field::new(name, self.arrow_type.clone(), false))),
            field_type: ParquetFieldType::Group {
                children: vec![self],
            },
        }
    }

    /// Returns a list of [`ParquetField`] children if this is a group type
    pub fn children(&self) -> Option<&[Self]> {
        match &self.field_type {
            ParquetFieldType::Primitive { .. } => None,
            ParquetFieldType::Group { children } => Some(children),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ParquetFieldType {
    Primitive {
        /// The index of the column in parquet
        col_idx: usize,
        /// The type of the column in parquet
        primitive_type: TypePtr,
    },
    Group {
        children: Vec<ParquetField>,
    },
}

pub struct ParquetRecordBatchReaderInner {
    batch_size: usize,
    array_reader: Box<dyn ArrayReader>,
    schema: SchemaRef,
    selection: Option<VecDeque<RowSelector>>,
}

impl ParquetRecordBatchReaderInner {
    pub fn new_parquet(
        batch_size: usize,
        array_reader: Box<dyn ArrayReader>,
        selection: Option<RowSelection>,
    ) -> parquet::arrow::arrow_reader::ParquetRecordBatchReader {
        let schema = match array_reader.get_data_type() {
            DataType::Struct(ref fields) => Schema::new(fields.clone()),
            _ => unreachable!("Struct array reader's data type is not struct!"),
        };

        let v = Self {
            batch_size,
            array_reader,
            schema: Arc::new(schema),
            selection: selection.map(|s| trim_row_selection(s).into()),
        };
        unsafe { std::mem::transmute(v) }
    }
}

fn trim_row_selection(selection: RowSelection) -> RowSelection {
    let mut selection: Vec<RowSelector> = selection.into();
    while selection.last().map(|x| x.skip).unwrap_or(false) {
        selection.pop();
    }
    RowSelection::from(selection)
}

pub(super) fn offset_row_selection(selection: RowSelection, offset: usize) -> RowSelection {
    if offset == 0 {
        return selection;
    }

    let mut selected_count = 0;
    let mut skipped_count = 0;

    let mut selectors: Vec<RowSelector> = selection.into();

    // Find the index where the selector exceeds the row count
    let find = selectors.iter().position(|selector| match selector.skip {
        true => {
            skipped_count += selector.row_count;
            false
        }
        false => {
            selected_count += selector.row_count;
            selected_count > offset
        }
    });

    let split_idx = match find {
        Some(idx) => idx,
        None => {
            selectors.clear();
            return RowSelection::from(selectors);
        }
    };

    let mut new_selectors = Vec::with_capacity(selectors.len() - split_idx + 1);
    new_selectors.push(RowSelector::skip(skipped_count + offset));
    new_selectors.push(RowSelector::select(selected_count - offset));
    new_selectors.extend_from_slice(&selectors[split_idx + 1..]);

    RowSelection::from(new_selectors)
}

pub(super) fn limit_row_selection(selection: RowSelection, mut limit: usize) -> RowSelection {
    let mut selectors: Vec<RowSelector> = selection.into();

    if limit == 0 {
        selectors.clear();
    }

    for (idx, selection) in selectors.iter_mut().enumerate() {
        if !selection.skip {
            if selection.row_count >= limit {
                selection.row_count = limit;
                selectors.truncate(idx + 1);
                break;
            } else {
                limit -= selection.row_count;
            }
        }
    }
    RowSelection::from(selectors)
}
