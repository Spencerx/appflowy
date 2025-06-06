use crate::entities::{
  CheckboxTypeOptionPB, ChecklistTypeOptionPB, DateTypeOptionPB, FieldType, MediaTypeOptionPB,
  MultiSelectTypeOptionPB, NumberTypeOptionPB, RelationTypeOptionPB, RichTextTypeOptionPB,
  SingleSelectTypeOptionPB, SummarizationTypeOptionPB, TimeTypeOptionPB, TimestampTypeOptionPB,
  TranslateTypeOptionPB, URLTypeOptionPB,
};
use crate::services::cell::CellDataDecoder;
use crate::services::filter::{ParseFilterData, PreFillCellsWithFilter};
use crate::services::sort::SortCondition;
use async_trait::async_trait;
use bytes::Bytes;
use collab_database::database::Database;
use collab_database::fields::checkbox_type_option::CheckboxTypeOption;
use collab_database::fields::checklist_type_option::ChecklistTypeOption;
use collab_database::fields::date_type_option::{DateTypeOption, TimeTypeOption};
use collab_database::fields::media_type_option::MediaTypeOption;
use collab_database::fields::number_type_option::NumberTypeOption;
use collab_database::fields::relation_type_option::RelationTypeOption;
use collab_database::fields::select_type_option::{MultiSelectTypeOption, SingleSelectTypeOption};
use collab_database::fields::summary_type_option::SummarizationTypeOption;
use collab_database::fields::text_type_option::RichTextTypeOption;
use collab_database::fields::timestamp_type_option::TimestampTypeOption;
use collab_database::fields::translate_type_option::TranslateTypeOption;
use collab_database::fields::url_type_option::URLTypeOption;
use collab_database::fields::{TypeOptionCellReader, TypeOptionData};
use collab_database::rows::Cell;
use collab_database::template::util::ToCellString;
pub use collab_database::template::util::TypeOptionCellData;
use protobuf::ProtobufError;
use std::cmp::Ordering;
use std::fmt::Debug;

pub trait TypeOption: From<TypeOptionData> + Into<TypeOptionData> + TypeOptionCellReader {
  /// `CellData` represents the decoded model for the current type option. Each of them must
  /// implement the From<&Cell> trait. If the `Cell` cannot be decoded into this type, the default
  /// value will be returned.
  ///
  /// Note: Use `StrCellData` for any `TypeOption` whose cell data is simply `String`.
  ///
  /// - FieldType::Checkbox => CheckboxCellData
  /// - FieldType::Date => DateCellData
  /// - FieldType::URL => URLCellData
  ///
  type CellData: for<'a> From<&'a Cell>
    + TypeOptionCellData
    + ToCellString
    + Default
    + Send
    + Sync
    + Clone
    + Debug
    + 'static;

  /// Represents as the corresponding field type cell changeset. Must be able
  /// to be placed into a `BoxAny`.
  ///
  type CellChangeset: Send + Sync + 'static;

  ///  For the moment, the protobuf type only be used in the FFI of `Dart`. If the decoded cell
  /// struct is just a `String`, then use the `StrCellData` as its `CellProtobufType`.
  /// Otherwise, providing a custom protobuf type as its `CellProtobufType`.
  /// For example:
  ///     FieldType::Date => DateCellDataPB
  ///     FieldType::URL => URLCellDataPB
  ///
  type CellProtobufType: TryInto<Bytes, Error = ProtobufError> + Debug;

  /// Represents the filter configuration for this type option.
  type CellFilter: ParseFilterData + PreFillCellsWithFilter + Clone + Send + Sync + 'static;
}
/// This trait providing serialization and deserialization methods for cell data.
///
/// This trait ensures that a type which implements both `TypeOption` and `TypeOptionCellDataSerde` can
/// be converted to and from a corresponding `Protobuf struct`, and can be parsed from an opaque [Cell] structure.
pub trait CellDataProtobufEncoder: TypeOption {
  /// Encode the cell data into corresponding `Protobuf struct`.
  /// For example:
  ///    FieldType::URL => URLCellDataPB
  ///    FieldType::Date=> DateCellDataPB
  fn protobuf_encode(
    &self,
    cell_data: <Self as TypeOption>::CellData,
  ) -> <Self as TypeOption>::CellProtobufType;
}

#[async_trait]
pub trait TypeOptionTransform: TypeOption + Send + Sync {
  /// Transform the TypeOption from one field type to another
  /// For example, when switching from `Checkbox` type option to `Single-Select`
  /// type option, adding the `Yes` option if the `Single-select` type-option doesn't contain it.
  /// But the cell content is a string, `Yes`, it's need to do the cell content transform.
  /// The `Yes` string will be transformed to the `Yes` option id.
  ///
  /// # Arguments
  ///
  /// * `old_type_option_field_type`: the FieldType of the passed-in TypeOption
  /// * `old_type_option_data`: the data that can be parsed into corresponding `TypeOption`.
  ///
  async fn transform_type_option(
    &mut self,
    _view_id: &str,
    _field_id: &str,
    _old_type_option_field_type: FieldType,
    _old_type_option_data: TypeOptionData,
    _new_type_option_field_type: FieldType,
    _database: &mut Database,
  ) {
  }
}

pub trait TypeOptionCellDataFilter: TypeOption + CellDataDecoder {
  fn apply_filter(
    &self,
    filter: &<Self as TypeOption>::CellFilter,
    cell_data: &<Self as TypeOption>::CellData,
  ) -> bool;
}

#[inline(always)]
pub fn default_order() -> Ordering {
  Ordering::Equal
}

pub trait TypeOptionCellDataCompare: TypeOption {
  /// Compares the cell contents of two cells that are both not
  /// None. However, the cell contents might still be empty
  fn apply_cmp(
    &self,
    cell_data: &<Self as TypeOption>::CellData,
    other_cell_data: &<Self as TypeOption>::CellData,
    sort_condition: SortCondition,
  ) -> Ordering;

  /// Compares the two cells where one of the cells is None
  fn apply_cmp_with_uninitialized(
    &self,
    cell_data: Option<&<Self as TypeOption>::CellData>,
    other_cell_data: Option<&<Self as TypeOption>::CellData>,
    _sort_condition: SortCondition,
  ) -> Ordering {
    match (cell_data, other_cell_data) {
      (None, Some(cell_data)) if !cell_data.is_cell_empty() => Ordering::Greater,
      (Some(cell_data), None) if !cell_data.is_cell_empty() => Ordering::Less,
      _ => Ordering::Equal,
    }
  }
}

pub fn type_option_data_from_pb<T: Into<Bytes>>(
  bytes: T,
  field_type: &FieldType,
) -> Result<TypeOptionData, ProtobufError> {
  let bytes = bytes.into();
  match field_type {
    FieldType::RichText => {
      RichTextTypeOptionPB::try_from(bytes).map(|pb| RichTextTypeOption::from(pb).into())
    },
    FieldType::Number => {
      NumberTypeOptionPB::try_from(bytes).map(|pb| NumberTypeOption::from(pb).into())
    },
    FieldType::DateTime => {
      DateTypeOptionPB::try_from(bytes).map(|pb| DateTypeOption::from(pb).into())
    },
    FieldType::LastEditedTime | FieldType::CreatedTime => {
      TimestampTypeOptionPB::try_from(bytes).map(|pb| TimestampTypeOption::from(pb).into())
    },
    FieldType::SingleSelect => {
      SingleSelectTypeOptionPB::try_from(bytes).map(|pb| SingleSelectTypeOption::from(pb).into())
    },
    FieldType::MultiSelect => {
      MultiSelectTypeOptionPB::try_from(bytes).map(|pb| MultiSelectTypeOption::from(pb).into())
    },
    FieldType::Checkbox => {
      CheckboxTypeOptionPB::try_from(bytes).map(|pb| CheckboxTypeOption::from(pb).into())
    },
    FieldType::URL => URLTypeOptionPB::try_from(bytes).map(|pb| URLTypeOption::from(pb).into()),
    FieldType::Checklist => {
      ChecklistTypeOptionPB::try_from(bytes).map(|pb| ChecklistTypeOption::from(pb).into())
    },
    FieldType::Relation => {
      RelationTypeOptionPB::try_from(bytes).map(|pb| RelationTypeOption::from(pb).into())
    },
    FieldType::Summary => {
      SummarizationTypeOptionPB::try_from(bytes).map(|pb| SummarizationTypeOption::from(pb).into())
    },
    FieldType::Time => TimeTypeOptionPB::try_from(bytes).map(|pb| TimeTypeOption::from(pb).into()),
    FieldType::Translate => {
      TranslateTypeOptionPB::try_from(bytes).map(|pb| TranslateTypeOption::from(pb).into())
    },
    FieldType::Media => {
      MediaTypeOptionPB::try_from(bytes).map(|pb| MediaTypeOption::from(pb).into())
    },
  }
}

pub fn type_option_to_pb(type_option: TypeOptionData, field_type: &FieldType) -> Bytes {
  match field_type {
    FieldType::RichText => {
      let rich_text_type_option: RichTextTypeOption = type_option.into();
      RichTextTypeOptionPB::from(rich_text_type_option)
        .try_into()
        .unwrap()
    },
    FieldType::Number => {
      let number_type_option: NumberTypeOption = type_option.into();
      NumberTypeOptionPB::from(number_type_option)
        .try_into()
        .unwrap()
    },
    FieldType::DateTime => {
      let date_type_option: DateTypeOption = type_option.into();
      DateTypeOptionPB::from(date_type_option).try_into().unwrap()
    },
    FieldType::LastEditedTime | FieldType::CreatedTime => {
      let timestamp_type_option: TimestampTypeOption = type_option.into();
      TimestampTypeOptionPB::from(timestamp_type_option)
        .try_into()
        .unwrap()
    },
    FieldType::SingleSelect => {
      let single_select_type_option: SingleSelectTypeOption = type_option.into();
      SingleSelectTypeOptionPB::from(single_select_type_option.0)
        .try_into()
        .unwrap()
    },
    FieldType::MultiSelect => {
      let multi_select_type_option: MultiSelectTypeOption = type_option.into();
      MultiSelectTypeOptionPB::from(multi_select_type_option.0)
        .try_into()
        .unwrap()
    },
    FieldType::Checkbox => {
      let checkbox_type_option: CheckboxTypeOption = type_option.into();
      CheckboxTypeOptionPB::from(checkbox_type_option)
        .try_into()
        .unwrap()
    },
    FieldType::URL => {
      let url_type_option: URLTypeOption = type_option.into();
      URLTypeOptionPB::from(url_type_option).try_into().unwrap()
    },
    FieldType::Checklist => {
      let checklist_type_option: ChecklistTypeOption = type_option.into();
      ChecklistTypeOptionPB::from(checklist_type_option)
        .try_into()
        .unwrap()
    },
    FieldType::Relation => {
      let relation_type_option: RelationTypeOption = type_option.into();
      RelationTypeOptionPB::from(relation_type_option)
        .try_into()
        .unwrap()
    },
    FieldType::Summary => {
      let summarization_type_option: SummarizationTypeOption = type_option.into();
      SummarizationTypeOptionPB::from(summarization_type_option)
        .try_into()
        .unwrap()
    },
    FieldType::Time => {
      let time_type_option: TimeTypeOption = type_option.into();
      TimeTypeOptionPB::from(time_type_option).try_into().unwrap()
    },
    FieldType::Translate => {
      let translate_type_option: TranslateTypeOption = type_option.into();
      TranslateTypeOptionPB::from(translate_type_option)
        .try_into()
        .unwrap()
    },
    FieldType::Media => {
      let media_type_option: MediaTypeOption = type_option.into();
      MediaTypeOptionPB::from(media_type_option)
        .try_into()
        .unwrap()
    },
  }
}

pub fn default_type_option_data_from_type(field_type: FieldType) -> TypeOptionData {
  match field_type {
    FieldType::RichText => RichTextTypeOption.into(),
    FieldType::Number => NumberTypeOption::default().into(),
    FieldType::DateTime => DateTypeOption::default().into(),
    FieldType::LastEditedTime | FieldType::CreatedTime => TimestampTypeOption {
      field_type: field_type.into(),
      ..Default::default()
    }
    .into(),
    FieldType::SingleSelect => SingleSelectTypeOption::default().into(),
    FieldType::MultiSelect => MultiSelectTypeOption::default().into(),
    FieldType::Checkbox => CheckboxTypeOption.into(),
    FieldType::URL => URLTypeOption::default().into(),
    FieldType::Checklist => ChecklistTypeOption.into(),
    FieldType::Relation => RelationTypeOption::default().into(),
    FieldType::Summary => SummarizationTypeOption::default().into(),
    FieldType::Translate => TranslateTypeOption::default().into(),
    FieldType::Time => TimeTypeOption.into(),
    FieldType::Media => MediaTypeOption::default().into(),
  }
}
