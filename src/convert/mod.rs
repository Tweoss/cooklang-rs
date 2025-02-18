//! Support for **configurable** unit conversion
//!
//! This includes:
//!     - A layered configuration system
//!     - Conversions between systems
//!     - Conversions to the best fit possible

use std::{collections::HashMap, ops::RangeInclusive, sync::Arc};

use enum_map::EnumMap;
use once_cell::sync::OnceCell;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    quantity::{Quantity, QuantityValue, Value},
    Recipe, UnitInfo,
};

pub use builder::{ConverterBuilder, ConverterBuilderError};
pub use units_file::UnitsFile;

use units_file::SIPrefix;

mod builder;
pub mod units_file;

/// Main struct to perform conversions
///
/// This holds information about all the known units and how to convert them.
///
/// To create one use [`Converter::builder`].
///
/// [`Converter::default`] changes with the feature `bundled_units`:
/// - When enabled, [`Converter::bundled`].
/// - When disabled, [`Converter::empty`].
#[derive(Debug, Clone)]
pub struct Converter {
    all_units: Vec<Arc<Unit>>,
    unit_index: UnitIndex,
    quantity_index: UnitQuantityIndex,
    best: EnumMap<PhysicalQuantity, BestConversionsStore>,
    default_system: System,

    temperature_regex: OnceCell<Regex>,
}

impl Converter {
    /// Start to create a new [Converter]
    pub fn builder() -> ConverterBuilder {
        ConverterBuilder::new()
    }

    /// Empty converter
    ///
    /// This is the default when the `bundled_units` feature is disabled.
    ///
    /// The main use case for this is to ignore the units, because an empty
    /// converter will fail to convert everything. Also, if the `ADVANCED_UNITS`
    /// extension is enabled, every timer unit will throw an error, because they
    /// have to be known time units.
    pub fn empty() -> Self {
        Self {
            all_units: Default::default(),
            unit_index: Default::default(),
            quantity_index: Default::default(),
            best: Default::default(),
            default_system: Default::default(),
            temperature_regex: Default::default(),
        }
    }

    /// Converter with the bundled units
    ///
    /// The converter will have the bundled units that doens't need any external
    /// file. These are the basic unit for most of the recipes you will need
    /// (in English).
    ///
    /// This is only available when the `bundled_units` feature is enabled.
    ///
    /// This is the default when the `bundled_units` feature is enabled.
    #[cfg(feature = "bundled_units")]
    pub fn bundled() -> Self {
        ConverterBuilder::new()
            .with_units_file(UnitsFile::bundled())
            .unwrap()
            .finish()
            .unwrap()
    }

    /// Get the default unit [System]
    pub fn default_system(&self) -> System {
        self.default_system
    }

    /// Get the total number of known units.
    ///
    /// This is **not** all the known unit names, just **different units**.
    pub fn unit_count(&self) -> usize {
        self.all_units.len()
    }

    /// Get a detailed count of the known units. See [UnitCount].
    pub fn unit_count_detailed(&self) -> UnitCount {
        UnitCount::new(self)
    }

    /// Get an iterator of all the known units.
    pub fn all_units(&self) -> impl Iterator<Item = &Unit> {
        self.all_units.iter().map(|u| u.as_ref())
    }

    /// Check if a unit is one of the possible conversions in it's units system.
    ///
    /// When a unit is a *best unit*, the converter can choose it when trying
    /// to get the best match for a value.
    ///
    /// # Panics
    /// If the unit is not known.
    pub fn is_best_unit(&self, unit: &Unit) -> bool {
        let unit_id = self
            .unit_index
            .get_unit_id(unit.symbol())
            .expect("unit not found");
        let mut iter = match &self.best[unit.physical_quantity] {
            BestConversionsStore::Unified(u) => u.0.iter(),
            BestConversionsStore::BySystem { metric, imperial } => match unit.system {
                Some(System::Metric) => metric.0.iter(),
                Some(System::Imperial) => imperial.0.iter(),
                None => return false,
            },
        };
        iter.any(|&(_, id)| id == unit_id)
    }
}

#[cfg(not(feature = "bundled_units"))]
impl Default for Converter {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(feature = "bundled_units")]
impl Default for Converter {
    fn default() -> Self {
        Self::bundled()
    }
}

impl PartialEq for Converter {
    fn eq(&self, other: &Self) -> bool {
        self.all_units == other.all_units
            && self.unit_index == other.unit_index
            && self.quantity_index == other.quantity_index
            && self.best == other.best
            && self.default_system == other.default_system
        // temperature_regex ignored, it should be the same if the rest is the
        // the same
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct UnitIndex(HashMap<Arc<str>, usize>);

pub(crate) type UnitQuantityIndex = EnumMap<PhysicalQuantity, Vec<usize>>;

/// A unit
///
/// Conversion will be `val * [Self::ratio] + [Self::difference]`
///
/// It implements [Display](std::fmt::Display). It will use [Self::symbol] or,
/// if alternate (`#`) is given, it will try the first name.
#[derive(Debug, Clone, Serialize)]
pub struct Unit {
    /// All the names that may be used to format the unit
    pub names: Vec<Arc<str>>,
    /// All the symbols (abbreviations), like `ml` for `millilitres`
    pub symbols: Vec<Arc<str>>,
    /// Custom aliases to parse the unit from a different string
    pub aliases: Vec<Arc<str>>,
    /// Conversion ratio
    pub ratio: f64,
    /// Difference offset to the conversion ratio
    pub difference: f64,
    /// The [PhysicalQuantity] this unit belongs to
    pub physical_quantity: PhysicalQuantity,
    /// The unit [System] this unit belongs to, if any
    pub system: Option<System>,
    #[serde(skip)]
    expand_si: bool,
    #[serde(skip)]
    expanded_units: Option<EnumMap<SIPrefix, usize>>,
}

impl Unit {
    fn all_keys(&self) -> impl Iterator<Item = &Arc<str>> {
        self.names.iter().chain(&self.symbols).chain(&self.aliases)
    }

    /// Get the symbol that represent this unit. The process is:
    /// - First symbol (if any)
    /// - Or first name (if any)
    /// - Or first alias (if any)
    /// - **panics**
    pub fn symbol(&self) -> &str {
        self.symbols
            .first()
            .or_else(|| self.names.first())
            .or_else(|| self.aliases.first())
            .expect("symbol, name or alias in unit")
    }
}

impl PartialEq for Unit {
    fn eq(&self, other: &Self) -> bool {
        self.names == other.names
            && self.symbols == other.symbols
            && self.aliases == other.aliases
            && self.ratio == other.ratio
            && self.difference == other.difference
            && self.physical_quantity == other.physical_quantity
            && self.system == other.system
        // expand_si and expanded_units ignored
    }
}

impl std::fmt::Display for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() && !self.names.is_empty() {
            write!(f, "{}", self.names[0])
        } else {
            write!(f, "{}", self.symbol())
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum BestConversionsStore {
    Unified(BestConversions),
    BySystem {
        metric: BestConversions,
        imperial: BestConversions,
    },
}

impl Default for BestConversionsStore {
    fn default() -> Self {
        Self::Unified(Default::default())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct BestConversions(Vec<(f64, usize)>);

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    Serialize,
    PartialOrd,
    Ord,
    strum::Display,
    strum::EnumString,
    enum_map::Enum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PhysicalQuantity {
    Volume,
    Mass,
    Length,
    Temperature,
    Time,
}

impl Converter {
    /// Convert a [Quantity]
    ///
    /// Just a convenience method of calling [Self::convert2]
    pub fn convert<'t>(
        &self,
        from: &Quantity,
        to: impl Into<ConvertTo<'t>>,
    ) -> Result<Quantity, ConvertError> {
        let to = to.into();
        self.convert_(from, to)
    }

    fn convert_(&self, from: &Quantity, to: ConvertTo) -> Result<Quantity, ConvertError> {
        let unit_info = from.unit().map(|u| u.unit_info_or_parse(self));
        let unit = match unit_info {
            Some(UnitInfo::Known(ref u)) => ConvertUnit::Unit(u),
            Some(UnitInfo::Unknown) => {
                return Err(ConvertError::UnknownUnit(UnknownUnit(
                    from.unit_text().unwrap().to_string(),
                )))
            }
            None => return Err(ConvertError::NoUnit(from.clone())),
        };

        let (value, unit) = match &from.value {
            QuantityValue::Fixed { value } => {
                let (value, unit) = self.convert2(value.try_into()?, unit, to)?;
                let q_value = QuantityValue::Fixed {
                    value: value.into(),
                };
                (q_value, unit)
            }
            QuantityValue::Linear { value } => {
                let (value, unit) = self.convert2(value.try_into()?, unit, to)?;
                let q_value = QuantityValue::Linear {
                    value: value.into(),
                };
                (q_value, unit)
            }
            QuantityValue::ByServings { values } => {
                let mut new_values = Vec::with_capacity(values.len());
                let mut new_unit = None;
                for v in values {
                    let (value, unit) = self.convert2(v.try_into()?, unit, to)?;
                    new_values.push(value.into());
                    new_unit = Some(unit);
                }
                let q_value = QuantityValue::ByServings { values: new_values };
                let unit = new_unit.expect("QuantityValue::ByServings empty");
                (q_value, unit)
            }
        };

        Ok(Quantity::with_known_unit(
            value,
            unit.to_string(),
            Some(unit),
        ))
    }

    /// Perform a conversion
    pub fn convert2(
        &self,
        value: ConvertValue,
        unit: ConvertUnit,
        to: ConvertTo,
    ) -> Result<(ConvertValue, Arc<Unit>), ConvertError> {
        let unit = self.get_unit(&unit)?;

        let (value, unit) = match to {
            ConvertTo::Unit(target_unit) => {
                let to = self.get_unit(&target_unit)?;
                let val = self.convert_to_unit(value, unit, to.as_ref())?;
                (val, Arc::clone(to))
            }
            ConvertTo::Best(system) => self.convert_to_best(value, unit, system)?,
            ConvertTo::SameSystem => {
                self.convert_to_best(value, unit, unit.system.unwrap_or(self.default_system))?
            }
        };
        Ok((value, unit))
    }

    fn convert_to_unit(
        &self,
        value: ConvertValue,
        unit: &Unit,
        target_unit: &Unit,
    ) -> Result<ConvertValue, ConvertError> {
        if unit.physical_quantity != target_unit.physical_quantity {
            return Err(ConvertError::MixedQuantities {
                from: unit.physical_quantity,
                to: target_unit.physical_quantity,
            });
        }
        Ok(self.convert_value(value, unit, target_unit))
    }

    fn convert_to_best(
        &self,
        value: ConvertValue,
        unit: &Unit,
        system: System,
    ) -> Result<(ConvertValue, Arc<Unit>), ConvertError> {
        let conversions = match &self.best[unit.physical_quantity] {
            BestConversionsStore::Unified(u) => u,
            BestConversionsStore::BySystem { metric, imperial } => match system {
                System::Metric => metric,
                System::Imperial => imperial,
            },
        };

        let best_unit = conversions.best_unit(self, &value, unit).ok_or({
            ConvertError::BestUnitNotFound {
                physical_quantity: unit.physical_quantity,
                system: unit.system,
            }
        })?;
        let converted = self.convert_value(value, unit, best_unit.as_ref());

        Ok((converted, best_unit))
    }

    fn convert_value(&self, value: ConvertValue, from: &Unit, to: &Unit) -> ConvertValue {
        match value {
            ConvertValue::Number(n) => ConvertValue::Number(self.convert_f64(n, from, to)),
            ConvertValue::Range(r) => {
                let s = self.convert_f64(*r.start(), from, to);
                let e = self.convert_f64(*r.end(), from, to);
                ConvertValue::Range(s..=e)
            }
        }
    }

    fn convert_f64(&self, value: f64, from: &Unit, to: &Unit) -> f64 {
        if std::ptr::eq(from, to) {
            return value;
        }
        convert_f64(value, from, to)
    }

    pub(crate) fn get_unit<'a>(
        &'a self,
        unit: &'a ConvertUnit,
    ) -> Result<&'a Arc<Unit>, UnknownUnit> {
        let unit = match unit {
            ConvertUnit::Unit(u) => u,
            ConvertUnit::Key(key) => {
                let id = self.unit_index.get_unit_id(key)?;
                &self.all_units[id]
            }
        };
        Ok(unit)
    }
}

pub(crate) fn convert_f64(value: f64, from: &Unit, to: &Unit) -> f64 {
    assert_eq!(from.physical_quantity, to.physical_quantity);

    let norm = (value + from.difference) * from.ratio;
    (norm / to.ratio) - to.difference
}

/// Error when try to convert an unknown unit
#[derive(Debug, Error)]
#[error("Unknown unit: '{0}'")]
pub struct UnknownUnit(String);

impl UnitIndex {
    fn get_unit_id(&self, key: &str) -> Result<usize, UnknownUnit> {
        self.0
            .get(key)
            .copied()
            .ok_or_else(|| UnknownUnit(key.to_string()))
    }
}

impl BestConversions {
    fn base(&self) -> Option<usize> {
        self.0.first().map(|c| c.1)
    }

    fn best_unit(
        &self,
        converter: &Converter,
        value: &ConvertValue,
        unit: &Unit,
    ) -> Option<Arc<Unit>> {
        let value = match value {
            ConvertValue::Number(n) => n.abs(),
            ConvertValue::Range(r) => r.start().abs(),
        };
        let base_unit_id = self.base()?;
        let base_unit = &converter.all_units[base_unit_id];
        let norm = converter.convert_f64(value, unit, base_unit);

        let best_id = self
            .0
            .iter()
            .rev()
            .find(|(th, _)| norm >= *th)
            .or_else(|| self.0.first())
            .map(|&(_, id)| id)?;
        Some(Arc::clone(&converter.all_units[best_id]))
    }
}

/// Input value for [Converter::convert]
#[derive(PartialEq, Clone, Debug)]
pub enum ConvertValue {
    Number(f64),
    /// It will convert the range as if start and end were 2 calls to convert as
    /// a number
    Range(RangeInclusive<f64>),
}

/// Input unit for [Converter::convert]
#[derive(Debug, Clone, Copy)]
pub enum ConvertUnit<'a> {
    /// A unit directly
    ///
    /// This is a small optimization when you already know the unit instance,
    /// but [`ConvertUnit::Key`] will produce the same result with a fast
    /// lookup.
    Unit(&'a Arc<Unit>),
    /// Any name, symbol or alias to a unit
    Key(&'a str),
}

/// Input target for [Converter::convert]
#[derive(Debug, Clone, Copy)]
pub enum ConvertTo<'a> {
    SameSystem,
    Best(System),
    Unit(ConvertUnit<'a>),
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    Serialize,
    Default,
    PartialOrd,
    Ord,
    strum::Display,
    strum::EnumString,
    enum_map::Enum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum System {
    #[default]
    Metric,
    Imperial,
}

impl<'a> From<&'a str> for ConvertUnit<'a> {
    fn from(value: &'a str) -> Self {
        Self::Key(value)
    }
}

impl<'a> From<&'a Arc<Unit>> for ConvertUnit<'a> {
    fn from(value: &'a Arc<Unit>) -> Self {
        Self::Unit(value)
    }
}

impl<'a> From<&'a str> for ConvertTo<'a> {
    fn from(value: &'a str) -> Self {
        Self::Unit(ConvertUnit::Key(value))
    }
}

impl From<System> for ConvertTo<'_> {
    fn from(value: System) -> Self {
        Self::Best(value)
    }
}

impl<'a> From<&'a Arc<Unit>> for ConvertTo<'a> {
    fn from(value: &'a Arc<Unit>) -> Self {
        Self::Unit(value.into())
    }
}

impl From<ConvertValue> for Value {
    fn from(value: ConvertValue) -> Self {
        match value {
            ConvertValue::Number(n) => Self::Number { value: n },
            ConvertValue::Range(r) => Self::Range { value: r },
        }
    }
}

impl TryFrom<&Value> for ConvertValue {
    type Error = ConvertError;
    fn try_from(value: &Value) -> Result<Self, Self::Error> {
        let value = match value {
            Value::Number { value: n } => ConvertValue::Number(*n),
            Value::Range { value: r } => ConvertValue::Range(r.clone()),
            Value::Text { value: t } => return Err(ConvertError::TextValue(t.to_string())),
        };
        Ok(value)
    }
}

impl From<f64> for ConvertValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<RangeInclusive<f64>> for ConvertValue {
    fn from(value: RangeInclusive<f64>) -> Self {
        Self::Range(value)
    }
}

impl PartialOrd<Self> for ConvertValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        fn extract(v: &ConvertValue) -> f64 {
            match v {
                ConvertValue::Number(n) => *n,
                ConvertValue::Range(r) => *r.start(),
            }
        }
        let this = extract(self);
        let other = extract(other);
        this.partial_cmp(&other)
    }
}

/// Errors from converting
#[derive(Debug, Error)]
pub enum ConvertError {
    #[error("Tried to convert a value with no unit: {0}")]
    NoUnit(Quantity),

    #[error("Tried to convert a text value: {0}")]
    TextValue(String),

    #[error("Mixed physical quantities: {from} {to}")]
    MixedQuantities {
        from: PhysicalQuantity,
        to: PhysicalQuantity,
    },

    #[error("Could not find best unit for a {physical_quantity} unit. System: {system:?}")]
    BestUnitNotFound {
        physical_quantity: PhysicalQuantity,
        system: Option<System>,
    },

    #[error(transparent)]
    UnknownUnit(#[from] UnknownUnit),
}

impl Converter {
    pub(crate) fn quantity_units(
        &self,
        physical_quantity: PhysicalQuantity,
    ) -> impl Iterator<Item = &Unit> {
        self.quantity_index[physical_quantity]
            .iter()
            .map(|&id| self.all_units[id].as_ref())
    }

    pub(crate) fn temperature_regex(&self) -> Result<&Regex, regex::Error> {
        self.temperature_regex.get_or_try_init(|| {
            let _guard = tracing::trace_span!("temp_regex").entered();
            let symbols = self
                .quantity_units(crate::convert::PhysicalQuantity::Temperature)
                .flat_map(|unit| unit.symbols.iter())
                .map(|symbol| format!("({symbol})"))
                .collect::<Vec<_>>()
                .join("|");
            let float = r"[+-]?\d+([.,]\d+)?";
            RegexBuilder::new(&format!(r"({float})\s*({symbols})"))
                .size_limit(500_000)
                .build()
        })
    }
}

/// Detailed count of units
pub struct UnitCount {
    /// Total number of units
    pub all: usize,
    /// Number of units by system
    pub by_system: EnumMap<System, usize>,
    /// Number of units by quantity
    pub by_quantity: EnumMap<PhysicalQuantity, usize>,
}

impl UnitCount {
    /// Calcualte the unit count
    pub fn new(converter: &Converter) -> Self {
        Self {
            all: converter.all_units.len(),
            by_system: converter
                .all_units
                .iter()
                .fold(EnumMap::default(), |mut m, u| {
                    if let Some(s) = u.system {
                        m[s] += 1
                    }
                    m
                }),
            by_quantity: enum_map::enum_map! {
                q => converter.quantity_index[q].len()
            },
        }
    }
}

impl<D> Recipe<D> {
    /// Convert a [Recipe] to another [System] in place.
    ///
    /// When an error occurs, it is stored and the quantity stays the same.
    ///
    /// Returns all the errors while converting. These usually are missing units,
    /// unknown units or text values.
    pub fn convert(&mut self, to: System, converter: &Converter) -> Vec<ConvertError> {
        let mut errors = Vec::new();

        let mut conv = |q: &mut Quantity| match converter.convert(q, to) {
            Ok(cq) => *q = cq,
            Err(e) => errors.push(e),
        };

        for igr in &mut self.ingredients {
            if let Some(q) = &mut igr.quantity {
                conv(q);
            }
        }

        // cookware can't have units

        for timer in &mut self.timers {
            if let Some(q) = &mut timer.quantity {
                conv(q);
            }
        }

        for q in &mut self.inline_quantities {
            conv(q);
        }

        errors
    }
}
