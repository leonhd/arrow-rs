// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Defines take kernel for [Array]

use std::{ops::AddAssign, sync::Arc};

use crate::buffer::{Buffer, MutableBuffer};
use crate::compute::util::{
    take_value_indices_from_fixed_size_list, take_value_indices_from_list,
};
use crate::datatypes::*;
use crate::error::{ArrowError, Result};
use crate::util::bit_util;
use crate::{array::*, buffer::buffer_bin_and};

use num::{ToPrimitive, Zero};
use TimeUnit::*;

macro_rules! downcast_take {
    ($type: ty, $values: expr, $indices: expr) => {{
        let values = $values
            .as_any()
            .downcast_ref::<PrimitiveArray<$type>>()
            .expect("Unable to downcast to a primitive array");
        Ok(Arc::new(take_primitive::<$type, _>(&values, $indices)?))
    }};
}

macro_rules! downcast_dict_take {
    ($type: ty, $values: expr, $indices: expr) => {{
        let values = $values
            .as_any()
            .downcast_ref::<DictionaryArray<$type>>()
            .expect("Unable to downcast to a dictionary array");
        Ok(Arc::new(take_dict::<$type, _>(values, $indices)?))
    }};
}

/// Take elements by index from [Array], creating a new [Array] from those indexes.
///
/// ```text
/// ┌─────────────────┐      ┌─────────┐                              ┌─────────────────┐
/// │        A        │      │    0    │                              │        A        │
/// ├─────────────────┤      ├─────────┤                              ├─────────────────┤
/// │        D        │      │    2    │                              │        B        │
/// ├─────────────────┤      ├─────────┤   take(values, indicies)     ├─────────────────┤
/// │        B        │      │    3    │ ─────────────────────────▶   │        C        │
/// ├─────────────────┤      ├─────────┤                              ├─────────────────┤
/// │        C        │      │    1    │                              │        D        │
/// ├─────────────────┤      └─────────┘                              └─────────────────┘
/// │        E        │
/// └─────────────────┘
///    values array            indicies array                              result
/// ```
///
/// # Errors
/// This function errors whenever:
/// * An index cannot be casted to `usize` (typically 32 bit architectures)
/// * An index is out of bounds and `options` is set to check bounds.
/// # Safety
/// When `options` is not set to check bounds (default), taking indexes after `len` is undefined behavior.
/// # Examples
/// ```
/// use arrow::array::{StringArray, UInt32Array};
/// use arrow::error::Result;
/// use arrow::compute::take;
/// # fn main() -> Result<()> {
/// let values = StringArray::from(vec!["zero", "one", "two"]);
///
/// // Take items at index 2, and 1:
/// let indices = UInt32Array::from(vec![2, 1]);
/// let taken = take(&values, &indices, None)?;
/// let taken = taken.as_any().downcast_ref::<StringArray>().unwrap();
///
/// assert_eq!(*taken, StringArray::from(vec!["two", "one"]));
/// # Ok(())
/// # }
/// ```
pub fn take<IndexType>(
    values: &dyn Array,
    indices: &PrimitiveArray<IndexType>,
    options: Option<TakeOptions>,
) -> Result<ArrayRef>
where
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
{
    take_impl(values, indices, options)
}

fn take_impl<IndexType>(
    values: &dyn Array,
    indices: &PrimitiveArray<IndexType>,
    options: Option<TakeOptions>,
) -> Result<ArrayRef>
where
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
{
    let options = options.unwrap_or_default();
    if options.check_bounds {
        let len = values.len();
        if indices.null_count() > 0 {
            indices.iter().flatten().try_for_each(|index| {
                let ix = ToPrimitive::to_usize(&index).ok_or_else(|| {
                    ArrowError::ComputeError("Cast to usize failed".to_string())
                })?;
                if ix >= len {
                    return Err(ArrowError::ComputeError(
                        format!("Array index out of bounds, cannot get item at index {} from {} entries", ix, len))
                    );
                }
                Ok(())
            })?;
        } else {
            indices.values().iter().try_for_each(|index| {
                let ix = ToPrimitive::to_usize(index).ok_or_else(|| {
                    ArrowError::ComputeError("Cast to usize failed".to_string())
                })?;
                if ix >= len {
                    return Err(ArrowError::ComputeError(
                        format!("Array index out of bounds, cannot get item at index {} from {} entries", ix, len))
                    );
                }
                Ok(())
            })?
        }
    }
    match values.data_type() {
        DataType::Boolean => {
            let values = values.as_any().downcast_ref::<BooleanArray>().unwrap();
            Ok(Arc::new(take_boolean(values, indices)?))
        }
        DataType::Decimal(_, _) => {
            let decimal_values = values.as_any().downcast_ref::<DecimalArray>().unwrap();
            Ok(Arc::new(take_decimal128(decimal_values, indices)?))
        }
        DataType::Int8 => downcast_take!(Int8Type, values, indices),
        DataType::Int16 => downcast_take!(Int16Type, values, indices),
        DataType::Int32 => downcast_take!(Int32Type, values, indices),
        DataType::Int64 => downcast_take!(Int64Type, values, indices),
        DataType::UInt8 => downcast_take!(UInt8Type, values, indices),
        DataType::UInt16 => downcast_take!(UInt16Type, values, indices),
        DataType::UInt32 => downcast_take!(UInt32Type, values, indices),
        DataType::UInt64 => downcast_take!(UInt64Type, values, indices),
        DataType::Float32 => downcast_take!(Float32Type, values, indices),
        DataType::Float64 => downcast_take!(Float64Type, values, indices),
        DataType::Date32 => downcast_take!(Date32Type, values, indices),
        DataType::Date64 => downcast_take!(Date64Type, values, indices),
        DataType::Time32(Second) => downcast_take!(Time32SecondType, values, indices),
        DataType::Time32(Millisecond) => {
            downcast_take!(Time32MillisecondType, values, indices)
        }
        DataType::Time64(Microsecond) => {
            downcast_take!(Time64MicrosecondType, values, indices)
        }
        DataType::Time64(Nanosecond) => {
            downcast_take!(Time64NanosecondType, values, indices)
        }
        DataType::Timestamp(Second, _) => {
            downcast_take!(TimestampSecondType, values, indices)
        }
        DataType::Timestamp(Millisecond, _) => {
            downcast_take!(TimestampMillisecondType, values, indices)
        }
        DataType::Timestamp(Microsecond, _) => {
            downcast_take!(TimestampMicrosecondType, values, indices)
        }
        DataType::Timestamp(Nanosecond, _) => {
            downcast_take!(TimestampNanosecondType, values, indices)
        }
        DataType::Interval(IntervalUnit::YearMonth) => {
            downcast_take!(IntervalYearMonthType, values, indices)
        }
        DataType::Interval(IntervalUnit::DayTime) => {
            downcast_take!(IntervalDayTimeType, values, indices)
        }
        DataType::Interval(IntervalUnit::MonthDayNano) => {
            downcast_take!(IntervalMonthDayNanoType, values, indices)
        }
        DataType::Duration(TimeUnit::Second) => {
            downcast_take!(DurationSecondType, values, indices)
        }
        DataType::Duration(TimeUnit::Millisecond) => {
            downcast_take!(DurationMillisecondType, values, indices)
        }
        DataType::Duration(TimeUnit::Microsecond) => {
            downcast_take!(DurationMicrosecondType, values, indices)
        }
        DataType::Duration(TimeUnit::Nanosecond) => {
            downcast_take!(DurationNanosecondType, values, indices)
        }
        DataType::Utf8 => {
            let values = values
                .as_any()
                .downcast_ref::<GenericStringArray<i32>>()
                .unwrap();
            Ok(Arc::new(take_string::<i32, _>(values, indices)?))
        }
        DataType::LargeUtf8 => {
            let values = values
                .as_any()
                .downcast_ref::<GenericStringArray<i64>>()
                .unwrap();
            Ok(Arc::new(take_string::<i64, _>(values, indices)?))
        }
        DataType::List(_) => {
            let values = values
                .as_any()
                .downcast_ref::<GenericListArray<i32>>()
                .unwrap();
            Ok(Arc::new(take_list::<_, Int32Type>(values, indices)?))
        }
        DataType::LargeList(_) => {
            let values = values
                .as_any()
                .downcast_ref::<GenericListArray<i64>>()
                .unwrap();
            Ok(Arc::new(take_list::<_, Int64Type>(values, indices)?))
        }
        DataType::FixedSizeList(_, length) => {
            let values = values
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .unwrap();
            Ok(Arc::new(take_fixed_size_list(
                values,
                indices,
                *length as u32,
            )?))
        }
        DataType::Struct(fields) => {
            let struct_: &StructArray =
                values.as_any().downcast_ref::<StructArray>().unwrap();
            let arrays: Result<Vec<ArrayRef>> = struct_
                .columns()
                .iter()
                .map(|a| take_impl(a.as_ref(), indices, Some(options.clone())))
                .collect();
            let arrays = arrays?;
            let fields: Vec<(Field, ArrayRef)> =
                fields.clone().into_iter().zip(arrays).collect();

            // Create the null bit buffer.
            let is_valid: Buffer = indices
                .iter()
                .map(|index| {
                    if let Some(index) = index {
                        struct_.is_valid(ArrowNativeType::to_usize(&index).unwrap())
                    } else {
                        false
                    }
                })
                .collect();

            Ok(Arc::new(StructArray::from((fields, is_valid))) as ArrayRef)
        }
        DataType::Dictionary(key_type, _) => match key_type.as_ref() {
            DataType::Int8 => downcast_dict_take!(Int8Type, values, indices),
            DataType::Int16 => downcast_dict_take!(Int16Type, values, indices),
            DataType::Int32 => downcast_dict_take!(Int32Type, values, indices),
            DataType::Int64 => downcast_dict_take!(Int64Type, values, indices),
            DataType::UInt8 => downcast_dict_take!(UInt8Type, values, indices),
            DataType::UInt16 => downcast_dict_take!(UInt16Type, values, indices),
            DataType::UInt32 => downcast_dict_take!(UInt32Type, values, indices),
            DataType::UInt64 => downcast_dict_take!(UInt64Type, values, indices),
            t => unimplemented!("Take not supported for dictionary key type {:?}", t),
        },
        DataType::Binary => {
            let values = values
                .as_any()
                .downcast_ref::<GenericBinaryArray<i32>>()
                .unwrap();
            Ok(Arc::new(take_binary(values, indices)?))
        }
        DataType::LargeBinary => {
            let values = values
                .as_any()
                .downcast_ref::<GenericBinaryArray<i64>>()
                .unwrap();
            Ok(Arc::new(take_binary(values, indices)?))
        }
        DataType::FixedSizeBinary(_) => {
            let values = values
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .unwrap();
            Ok(Arc::new(take_fixed_size_binary(values, indices)?))
        }
        DataType::Null => {
            // Take applied to a null array produces a null array.
            if values.len() >= indices.len() {
                // If the existing null array is as big as the indices, we can use a slice of it
                // to avoid allocating a new null array.
                Ok(values.slice(0, indices.len()))
            } else {
                // If the existing null array isn't big enough, create a new one.
                Ok(new_null_array(&DataType::Null, indices.len()))
            }
        }
        t => unimplemented!("Take not supported for data type {:?}", t),
    }
}

/// Options that define how `take` should behave
#[derive(Clone, Debug, Default)]
pub struct TakeOptions {
    /// Perform bounds check before taking indices from values.
    /// If enabled, an `ArrowError` is returned if the indices are out of bounds.
    /// If not enabled, and indices exceed bounds, the kernel will panic.
    pub check_bounds: bool,
}

#[inline(always)]
fn maybe_usize<I: ArrowNativeType>(index: I) -> Result<usize> {
    index
        .to_usize()
        .ok_or_else(|| ArrowError::ComputeError("Cast to usize failed".to_string()))
}

// take implementation when neither values nor indices contain nulls
fn take_no_nulls<T, I>(values: &[T], indices: &[I]) -> Result<(Buffer, Option<Buffer>)>
where
    T: ArrowNativeType,
    I: ArrowNativeType,
{
    let values = indices
        .iter()
        .map(|index| Result::Ok(values[maybe_usize::<I>(*index)?]));
    // Soundness: `slice.map` is `TrustedLen`.
    let buffer = unsafe { Buffer::try_from_trusted_len_iter(values)? };

    Ok((buffer, None))
}

// take implementation when only values contain nulls
fn take_values_nulls<T, I>(
    values: &PrimitiveArray<T>,
    indices: &[I],
) -> Result<(Buffer, Option<Buffer>)>
where
    T: ArrowPrimitiveType,
    I: ArrowNativeType,
{
    take_values_nulls_inner(values.data(), values.values(), indices)
}

fn take_values_nulls_inner<T, I>(
    values_data: &ArrayData,
    values: &[T],
    indices: &[I],
) -> Result<(Buffer, Option<Buffer>)>
where
    T: ArrowNativeType,
    I: ArrowNativeType,
{
    let num_bytes = bit_util::ceil(indices.len(), 8);
    let mut nulls = MutableBuffer::new(num_bytes).with_bitset(num_bytes, true);
    let null_slice = nulls.as_slice_mut();
    let mut null_count = 0;

    let values = indices.iter().enumerate().map(|(i, index)| {
        let index = maybe_usize::<I>(*index)?;
        if values_data.is_null(index) {
            null_count += 1;
            bit_util::unset_bit(null_slice, i);
        }
        Result::Ok(values[index])
    });
    // Soundness: `slice.map` is `TrustedLen`.
    let buffer = unsafe { Buffer::try_from_trusted_len_iter(values)? };

    let nulls = if null_count == 0 {
        // if only non-null values were taken
        None
    } else {
        Some(nulls.into())
    };

    Ok((buffer, nulls))
}

// take implementation when only indices contain nulls
fn take_indices_nulls<T, I>(
    values: &[T],
    indices: &PrimitiveArray<I>,
) -> Result<(Buffer, Option<Buffer>)>
where
    T: ArrowNativeType,
    I: ArrowNumericType,
    I::Native: ToPrimitive,
{
    take_indices_nulls_inner(values, indices.values(), indices.data())
}

fn take_indices_nulls_inner<T, I>(
    values: &[T],
    indices: &[I],
    indices_data: &ArrayData,
) -> Result<(Buffer, Option<Buffer>)>
where
    T: ArrowNativeType,
    I: ArrowNativeType,
{
    let values = indices.iter().map(|index| {
        let index = maybe_usize::<I>(*index)?;
        Result::Ok(match values.get(index) {
            Some(value) => *value,
            None => {
                if indices_data.is_null(index) {
                    T::default()
                } else {
                    panic!("Out-of-bounds index {}", index)
                }
            }
        })
    });

    // Soundness: `slice.map` is `TrustedLen`.
    let buffer = unsafe { Buffer::try_from_trusted_len_iter(values)? };

    Ok((
        buffer,
        indices_data
            .null_buffer()
            .map(|b| b.bit_slice(indices_data.offset(), indices.len())),
    ))
}

// take implementation when both values and indices contain nulls
fn take_values_indices_nulls<T, I>(
    values: &PrimitiveArray<T>,
    indices: &PrimitiveArray<I>,
) -> Result<(Buffer, Option<Buffer>)>
where
    T: ArrowPrimitiveType,
    I: ArrowNumericType,
    I::Native: ToPrimitive,
{
    take_values_indices_nulls_inner(
        values.values(),
        values.data(),
        indices.values(),
        indices.data(),
    )
}

fn take_values_indices_nulls_inner<T, I>(
    values: &[T],
    values_data: &ArrayData,
    indices: &[I],
    indices_data: &ArrayData,
) -> Result<(Buffer, Option<Buffer>)>
where
    T: ArrowNativeType,
    I: ArrowNativeType,
{
    let num_bytes = bit_util::ceil(indices.len(), 8);
    let mut nulls = MutableBuffer::new(num_bytes).with_bitset(num_bytes, true);
    let null_slice = nulls.as_slice_mut();
    let mut null_count = 0;

    let values = indices.iter().enumerate().map(|(i, &index)| {
        if indices_data.is_null(i) {
            null_count += 1;
            bit_util::unset_bit(null_slice, i);
            Ok(T::default())
        } else {
            let index = maybe_usize::<I>(index)?;
            if values_data.is_null(index) {
                null_count += 1;
                bit_util::unset_bit(null_slice, i);
            }
            Result::Ok(values[index])
        }
    });
    // Soundness: `slice.map` is `TrustedLen`.
    let buffer = unsafe { Buffer::try_from_trusted_len_iter(values)? };

    let nulls = if null_count == 0 {
        // if only non-null values were taken
        None
    } else {
        Some(nulls.into())
    };

    Ok((buffer, nulls))
}

/// `take` implementation for decimal arrays
fn take_decimal128<IndexType>(
    decimal_values: &DecimalArray,
    indices: &PrimitiveArray<IndexType>,
) -> Result<DecimalArray>
where
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
{
    indices
        .iter()
        .map(|index| {
            // Use type annotations below for readability (was blowing
            // my mind otherwise)
            let t: Option<Result<Option<_>>> = index.map(|index| {
                let index = ToPrimitive::to_usize(&index).ok_or_else(|| {
                    ArrowError::ComputeError("Cast to usize failed".to_string())
                })?;

                if decimal_values.is_null(index) {
                    Ok(None)
                } else {
                    Ok(Some(decimal_values.value(index).as_i128()))
                }
            });
            let t: Result<Option<Option<_>>> = t.transpose();
            let t: Result<Option<_>> = t.map(|t| t.flatten());
            t
        })
        .collect::<Result<DecimalArray>>()?
        // PERF: we could avoid re-validating that the data in
        // DecimalArray was in range as we know it came from a valid DecimalArray
        .with_precision_and_scale(decimal_values.precision(), decimal_values.scale())
}

/// `take` implementation for all primitive arrays
///
/// This checks if an `indices` slot is populated, and gets the value from `values`
///  as the populated index.
/// If the `indices` slot is null, a null value is returned.
/// For example, given:
///     values:  [1, 2, 3, null, 5]
///     indices: [0, null, 4, 3]
/// The result is: [1 (slot 0), null (null slot), 5 (slot 4), null (slot 3)]
fn take_primitive<T, I>(
    values: &PrimitiveArray<T>,
    indices: &PrimitiveArray<I>,
) -> Result<PrimitiveArray<T>>
where
    T: ArrowPrimitiveType,
    I: ArrowNumericType,
    I::Native: ToPrimitive,
{
    let indices_has_nulls = indices.null_count() > 0;
    let values_has_nulls = values.null_count() > 0;
    // note: this function should only panic when "an index is not null and out of bounds".
    // if the index is null, its value is undefined and therefore we should not read from it.

    let (buffer, nulls) = match (values_has_nulls, indices_has_nulls) {
        (false, false) => {
            // * no nulls
            // * all `indices.values()` are valid
            take_no_nulls::<T::Native, I::Native>(values.values(), indices.values())?
        }
        (true, false) => {
            // * nulls come from `values` alone
            // * all `indices.values()` are valid
            take_values_nulls::<T, I::Native>(values, indices.values())?
        }
        (false, true) => {
            // in this branch it is unsound to read and use `index.values()`,
            // as doing so is UB when they come from a null slot.
            take_indices_nulls::<T::Native, I>(values.values(), indices)?
        }
        (true, true) => {
            // in this branch it is unsound to read and use `index.values()`,
            // as doing so is UB when they come from a null slot.
            take_values_indices_nulls::<T, I>(values, indices)?
        }
    };

    let data = unsafe {
        ArrayData::new_unchecked(
            values.data_type().clone(),
            indices.len(),
            None,
            nulls,
            0,
            vec![buffer],
            vec![],
        )
    };
    Ok(PrimitiveArray::<T>::from(data))
}

/// `take` implementation for boolean arrays
fn take_boolean<IndexType>(
    values: &BooleanArray,
    indices: &PrimitiveArray<IndexType>,
) -> Result<BooleanArray>
where
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
{
    let data_len = indices.len();

    let num_byte = bit_util::ceil(data_len, 8);
    let mut val_buf = MutableBuffer::from_len_zeroed(num_byte);

    let val_slice = val_buf.as_slice_mut();

    let null_count = values.null_count();

    let nulls = if null_count == 0 {
        (0..data_len).try_for_each::<_, Result<()>>(|i| {
            let index = ToPrimitive::to_usize(&indices.value(i)).ok_or_else(|| {
                ArrowError::ComputeError("Cast to usize failed".to_string())
            })?;

            if values.value(index) {
                bit_util::set_bit(val_slice, i);
            }

            Ok(())
        })?;

        indices.data_ref().null_buffer().cloned()
    } else {
        let mut null_buf = MutableBuffer::new(num_byte).with_bitset(num_byte, true);
        let null_slice = null_buf.as_slice_mut();

        (0..data_len).try_for_each::<_, Result<()>>(|i| {
            let index = ToPrimitive::to_usize(&indices.value(i)).ok_or_else(|| {
                ArrowError::ComputeError("Cast to usize failed".to_string())
            })?;

            if values.is_null(index) {
                bit_util::unset_bit(null_slice, i);
            } else if values.value(index) {
                bit_util::set_bit(val_slice, i);
            }

            Ok(())
        })?;

        match indices.data_ref().null_buffer() {
            Some(buffer) => Some(buffer_bin_and(
                buffer,
                indices.offset(),
                &null_buf.into(),
                0,
                indices.len(),
            )),
            None => Some(null_buf.into()),
        }
    };

    let data = unsafe {
        ArrayData::new_unchecked(
            DataType::Boolean,
            indices.len(),
            None,
            nulls,
            0,
            vec![val_buf.into()],
            vec![],
        )
    };
    Ok(BooleanArray::from(data))
}

/// `take` implementation for string arrays
fn take_string<OffsetSize, IndexType>(
    array: &GenericStringArray<OffsetSize>,
    indices: &PrimitiveArray<IndexType>,
) -> Result<GenericStringArray<OffsetSize>>
where
    OffsetSize: Zero + AddAssign + OffsetSizeTrait,
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
{
    let data_len = indices.len();

    let bytes_offset = (data_len + 1) * std::mem::size_of::<OffsetSize>();
    let mut offsets_buffer = MutableBuffer::from_len_zeroed(bytes_offset);

    let offsets = offsets_buffer.typed_data_mut();
    let mut values = MutableBuffer::new(0);
    let mut length_so_far = OffsetSize::zero();
    offsets[0] = length_so_far;

    let nulls;
    if array.null_count() == 0 && indices.null_count() == 0 {
        for (i, offset) in offsets.iter_mut().skip(1).enumerate() {
            let index = ToPrimitive::to_usize(&indices.value(i)).ok_or_else(|| {
                ArrowError::ComputeError("Cast to usize failed".to_string())
            })?;

            let s = array.value(index);

            length_so_far += OffsetSize::from_usize(s.len()).unwrap();
            values.extend_from_slice(s.as_bytes());
            *offset = length_so_far;
        }
        nulls = None
    } else if indices.null_count() == 0 {
        let num_bytes = bit_util::ceil(data_len, 8);

        let mut null_buf = MutableBuffer::new(num_bytes).with_bitset(num_bytes, true);
        let null_slice = null_buf.as_slice_mut();

        for (i, offset) in offsets.iter_mut().skip(1).enumerate() {
            let index = ToPrimitive::to_usize(&indices.value(i)).ok_or_else(|| {
                ArrowError::ComputeError("Cast to usize failed".to_string())
            })?;

            if array.is_valid(index) {
                let s = array.value(index);

                length_so_far += OffsetSize::from_usize(s.len()).unwrap();
                values.extend_from_slice(s.as_bytes());
            } else {
                bit_util::unset_bit(null_slice, i);
            }
            *offset = length_so_far;
        }
        nulls = Some(null_buf.into());
    } else if array.null_count() == 0 {
        for (i, offset) in offsets.iter_mut().skip(1).enumerate() {
            if indices.is_valid(i) {
                let index =
                    ToPrimitive::to_usize(&indices.value(i)).ok_or_else(|| {
                        ArrowError::ComputeError("Cast to usize failed".to_string())
                    })?;

                let s = array.value(index);

                length_so_far += OffsetSize::from_usize(s.len()).unwrap();
                values.extend_from_slice(s.as_bytes());
            }
            *offset = length_so_far;
        }
        nulls = indices.data_ref().null_buffer().cloned();
    } else {
        let num_bytes = bit_util::ceil(data_len, 8);

        let mut null_buf = MutableBuffer::new(num_bytes).with_bitset(num_bytes, true);
        let null_slice = null_buf.as_slice_mut();

        for (i, offset) in offsets.iter_mut().skip(1).enumerate() {
            let index = ToPrimitive::to_usize(&indices.value(i)).ok_or_else(|| {
                ArrowError::ComputeError("Cast to usize failed".to_string())
            })?;

            if array.is_valid(index) && indices.is_valid(i) {
                let s = array.value(index);

                length_so_far += OffsetSize::from_usize(s.len()).unwrap();
                values.extend_from_slice(s.as_bytes());
            } else {
                // set null bit
                bit_util::unset_bit(null_slice, i);
            }
            *offset = length_so_far;
        }

        nulls = match indices.data_ref().null_buffer() {
            Some(buffer) => {
                Some(buffer_bin_and(buffer, 0, &null_buf.into(), 0, data_len))
            }
            None => Some(null_buf.into()),
        };
    }

    let array_data =
        ArrayData::builder(GenericStringArray::<OffsetSize>::get_data_type())
            .len(data_len)
            .add_buffer(offsets_buffer.into())
            .add_buffer(values.into())
            .null_bit_buffer(nulls);

    let array_data = unsafe { array_data.build_unchecked() };

    Ok(GenericStringArray::<OffsetSize>::from(array_data))
}

/// `take` implementation for list arrays
///
/// Calculates the index and indexed offset for the inner array,
/// applying `take` on the inner array, then reconstructing a list array
/// with the indexed offsets
fn take_list<IndexType, OffsetType>(
    values: &GenericListArray<OffsetType::Native>,
    indices: &PrimitiveArray<IndexType>,
) -> Result<GenericListArray<OffsetType::Native>>
where
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
    OffsetType: ArrowNumericType,
    OffsetType::Native: ToPrimitive + OffsetSizeTrait,
    PrimitiveArray<OffsetType>: From<Vec<Option<OffsetType::Native>>>,
{
    // TODO: Some optimizations can be done here such as if it is
    // taking the whole list or a contiguous sublist
    let (list_indices, offsets) =
        take_value_indices_from_list::<IndexType, OffsetType>(values, indices)?;

    let taken = take_impl::<OffsetType>(values.values().as_ref(), &list_indices, None)?;
    // determine null count and null buffer, which are a function of `values` and `indices`
    let mut null_count = 0;
    let num_bytes = bit_util::ceil(indices.len(), 8);
    let mut null_buf = MutableBuffer::new(num_bytes).with_bitset(num_bytes, true);
    {
        let null_slice = null_buf.as_slice_mut();
        offsets[..].windows(2).enumerate().for_each(
            |(i, window): (usize, &[OffsetType::Native])| {
                if window[0] == window[1] {
                    // offsets are equal, slot is null
                    bit_util::unset_bit(null_slice, i);
                    null_count += 1;
                }
            },
        );
    }
    let value_offsets = Buffer::from_slice_ref(&offsets);
    // create a new list with taken data and computed null information
    let list_data = ArrayDataBuilder::new(values.data_type().clone())
        .len(indices.len())
        .null_bit_buffer(Some(null_buf.into()))
        .offset(0)
        .add_child_data(taken.data().clone())
        .add_buffer(value_offsets);

    let list_data = unsafe { list_data.build_unchecked() };

    Ok(GenericListArray::<OffsetType::Native>::from(list_data))
}

/// `take` implementation for `FixedSizeListArray`
///
/// Calculates the index and indexed offset for the inner array,
/// applying `take` on the inner array, then reconstructing a list array
/// with the indexed offsets
fn take_fixed_size_list<IndexType>(
    values: &FixedSizeListArray,
    indices: &PrimitiveArray<IndexType>,
    length: <UInt32Type as ArrowPrimitiveType>::Native,
) -> Result<FixedSizeListArray>
where
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
{
    let list_indices = take_value_indices_from_fixed_size_list(values, indices, length)?;
    let taken = take_impl::<UInt32Type>(values.values().as_ref(), &list_indices, None)?;

    // determine null count and null buffer, which are a function of `values` and `indices`
    let num_bytes = bit_util::ceil(indices.len(), 8);
    let mut null_buf = MutableBuffer::new(num_bytes).with_bitset(num_bytes, true);
    let null_slice = null_buf.as_slice_mut();

    for i in 0..indices.len() {
        let index = ToPrimitive::to_usize(&indices.value(i)).ok_or_else(|| {
            ArrowError::ComputeError("Cast to usize failed".to_string())
        })?;
        if !indices.is_valid(i) || values.is_null(index) {
            bit_util::unset_bit(null_slice, i);
        }
    }

    let list_data = ArrayDataBuilder::new(values.data_type().clone())
        .len(indices.len())
        .null_bit_buffer(Some(null_buf.into()))
        .offset(0)
        .add_child_data(taken.data().clone());

    let list_data = unsafe { list_data.build_unchecked() };

    Ok(FixedSizeListArray::from(list_data))
}

fn take_binary<IndexType, OffsetType>(
    values: &GenericBinaryArray<OffsetType>,
    indices: &PrimitiveArray<IndexType>,
) -> Result<GenericBinaryArray<OffsetType>>
where
    OffsetType: OffsetSizeTrait,
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
{
    let data_ref = values.data_ref();
    let array_iter = indices
        .values()
        .iter()
        .map(|idx| {
            let idx = maybe_usize::<IndexType::Native>(*idx)?;
            if data_ref.is_valid(idx) {
                Ok(Some(values.value(idx)))
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter();

    Ok(array_iter.collect::<GenericBinaryArray<OffsetType>>())
}

fn take_fixed_size_binary<IndexType>(
    values: &FixedSizeBinaryArray,
    indices: &PrimitiveArray<IndexType>,
) -> Result<FixedSizeBinaryArray>
where
    IndexType: ArrowNumericType,
    IndexType::Native: ToPrimitive,
{
    let data_ref = values.data_ref();
    let array_iter = indices
        .values()
        .iter()
        .map(|idx| {
            let idx = maybe_usize::<IndexType::Native>(*idx)?;
            if data_ref.is_valid(idx) {
                Ok(Some(values.value(idx)))
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter();

    FixedSizeBinaryArray::try_from_sparse_iter(array_iter)
}

/// `take` implementation for dictionary arrays
///
/// applies `take` to the keys of the dictionary array and returns a new dictionary array
/// with the same dictionary values and reordered keys
fn take_dict<T, I>(
    values: &DictionaryArray<T>,
    indices: &PrimitiveArray<I>,
) -> Result<DictionaryArray<T>>
where
    T: ArrowPrimitiveType,
    T::Native: num::Num,
    I: ArrowNumericType,
    I::Native: ToPrimitive,
{
    let new_keys = take_primitive::<T, I>(values.keys(), indices)?;
    let new_keys_data = new_keys.data_ref();

    let data = unsafe {
        ArrayData::new_unchecked(
            values.data_type().clone(),
            new_keys.len(),
            Some(new_keys_data.null_count()),
            new_keys_data.null_buffer().cloned(),
            0,
            new_keys_data.buffers().to_vec(),
            values.data().child_data().to_vec(),
        )
    };

    Ok(DictionaryArray::<T>::from(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::util::tests::build_fixed_size_list_nullable;

    fn test_take_decimal_arrays(
        data: Vec<Option<i128>>,
        index: &UInt32Array,
        options: Option<TakeOptions>,
        expected_data: Vec<Option<i128>>,
        precision: &usize,
        scale: &usize,
    ) -> Result<()> {
        let output = data
            .into_iter()
            .collect::<DecimalArray>()
            .with_precision_and_scale(*precision, *scale)
            .unwrap();

        let expected = expected_data
            .into_iter()
            .collect::<DecimalArray>()
            .with_precision_and_scale(*precision, *scale)
            .unwrap();

        let expected = Arc::new(expected) as ArrayRef;
        let output = take(&output, index, options).unwrap();
        assert_eq!(&output, &expected);
        Ok(())
    }

    fn test_take_boolean_arrays(
        data: Vec<Option<bool>>,
        index: &UInt32Array,
        options: Option<TakeOptions>,
        expected_data: Vec<Option<bool>>,
    ) {
        let output = BooleanArray::from(data);
        let expected = Arc::new(BooleanArray::from(expected_data)) as ArrayRef;
        let output = take(&output, index, options).unwrap();
        assert_eq!(&output, &expected)
    }

    fn test_take_primitive_arrays<T>(
        data: Vec<Option<T::Native>>,
        index: &UInt32Array,
        options: Option<TakeOptions>,
        expected_data: Vec<Option<T::Native>>,
    ) -> Result<()>
    where
        T: ArrowPrimitiveType,
        PrimitiveArray<T>: From<Vec<Option<T::Native>>>,
    {
        let output = PrimitiveArray::<T>::from(data);
        let expected = Arc::new(PrimitiveArray::<T>::from(expected_data)) as ArrayRef;
        let output = take(&output, index, options)?;
        assert_eq!(&output, &expected);
        Ok(())
    }

    fn test_take_primitive_arrays_non_null<T>(
        data: Vec<T::Native>,
        index: &UInt32Array,
        options: Option<TakeOptions>,
        expected_data: Vec<Option<T::Native>>,
    ) -> Result<()>
    where
        T: ArrowPrimitiveType,
        PrimitiveArray<T>: From<Vec<T::Native>>,
        PrimitiveArray<T>: From<Vec<Option<T::Native>>>,
    {
        let output = PrimitiveArray::<T>::from(data);
        let expected = Arc::new(PrimitiveArray::<T>::from(expected_data)) as ArrayRef;
        let output = take(&output, index, options)?;
        assert_eq!(&output, &expected);
        Ok(())
    }

    fn test_take_impl_primitive_arrays<T, I>(
        data: Vec<Option<T::Native>>,
        index: &PrimitiveArray<I>,
        options: Option<TakeOptions>,
        expected_data: Vec<Option<T::Native>>,
    ) where
        T: ArrowPrimitiveType,
        PrimitiveArray<T>: From<Vec<Option<T::Native>>>,
        I: ArrowNumericType,
        I::Native: ToPrimitive,
    {
        let output = PrimitiveArray::<T>::from(data);
        let expected = PrimitiveArray::<T>::from(expected_data);
        let output = take_impl(&output, index, options).unwrap();
        let output = output.as_any().downcast_ref::<PrimitiveArray<T>>().unwrap();
        assert_eq!(output, &expected)
    }

    // create a simple struct for testing purposes
    fn create_test_struct(
        values: Vec<Option<(Option<bool>, Option<i32>)>>,
    ) -> StructArray {
        let mut struct_builder = StructBuilder::new(
            vec![
                Field::new("a", DataType::Boolean, true),
                Field::new("b", DataType::Int32, true),
            ],
            vec![
                Box::new(BooleanBuilder::new(values.len())),
                Box::new(Int32Builder::new(values.len())),
            ],
        );

        for value in values {
            struct_builder
                .field_builder::<BooleanBuilder>(0)
                .unwrap()
                .append_option(value.and_then(|v| v.0))
                .unwrap();
            struct_builder
                .field_builder::<Int32Builder>(1)
                .unwrap()
                .append_option(value.and_then(|v| v.1))
                .unwrap();
            struct_builder.append(value.is_some()).unwrap();
        }
        struct_builder.finish()
    }

    #[test]
    fn test_take_decimal128_non_null_indices() {
        let index = UInt32Array::from(vec![0, 5, 3, 1, 4, 2]);
        let precision: usize = 10;
        let scale: usize = 5;
        test_take_decimal_arrays(
            vec![None, Some(3), Some(5), Some(2), Some(3), None],
            &index,
            None,
            vec![None, None, Some(2), Some(3), Some(3), Some(5)],
            &precision,
            &scale,
        )
        .unwrap();
    }

    #[test]
    fn test_take_decimal128() {
        let index = UInt32Array::from(vec![Some(3), None, Some(1), Some(3), Some(2)]);
        let precision: usize = 10;
        let scale: usize = 5;
        test_take_decimal_arrays(
            vec![Some(0), Some(1), Some(2), Some(3), Some(4)],
            &index,
            None,
            vec![Some(3), None, Some(1), Some(3), Some(2)],
            &precision,
            &scale,
        )
        .unwrap();
    }

    #[test]
    fn test_take_primitive_non_null_indices() {
        let index = UInt32Array::from(vec![0, 5, 3, 1, 4, 2]);
        test_take_primitive_arrays::<Int8Type>(
            vec![None, Some(3), Some(5), Some(2), Some(3), None],
            &index,
            None,
            vec![None, None, Some(2), Some(3), Some(3), Some(5)],
        )
        .unwrap();
    }

    #[test]
    fn test_take_primitive_non_null_values() {
        let index = UInt32Array::from(vec![Some(3), None, Some(1), Some(3), Some(2)]);
        test_take_primitive_arrays::<Int8Type>(
            vec![Some(0), Some(1), Some(2), Some(3), Some(4)],
            &index,
            None,
            vec![Some(3), None, Some(1), Some(3), Some(2)],
        )
        .unwrap();
    }

    #[test]
    fn test_take_primitive_non_null() {
        let index = UInt32Array::from(vec![0, 5, 3, 1, 4, 2]);
        test_take_primitive_arrays::<Int8Type>(
            vec![Some(0), Some(3), Some(5), Some(2), Some(3), Some(1)],
            &index,
            None,
            vec![Some(0), Some(1), Some(2), Some(3), Some(3), Some(5)],
        )
        .unwrap();
    }

    #[test]
    fn test_take_primitive_nullable_indices_non_null_values_with_offset() {
        let index =
            UInt32Array::from(vec![Some(0), Some(1), Some(2), Some(3), None, None]);
        let index = index.slice(2, 4);
        let index = index.as_any().downcast_ref::<UInt32Array>().unwrap();

        assert_eq!(
            index,
            &UInt32Array::from(vec![Some(2), Some(3), None, None])
        );

        test_take_primitive_arrays_non_null::<Int64Type>(
            vec![0, 10, 20, 30, 40, 50],
            index,
            None,
            vec![Some(20), Some(30), None, None],
        )
        .unwrap();
    }

    #[test]
    fn test_take_primitive_nullable_indices_nullable_values_with_offset() {
        let index =
            UInt32Array::from(vec![Some(0), Some(1), Some(2), Some(3), None, None]);
        let index = index.slice(2, 4);
        let index = index.as_any().downcast_ref::<UInt32Array>().unwrap();

        assert_eq!(
            index,
            &UInt32Array::from(vec![Some(2), Some(3), None, None])
        );

        test_take_primitive_arrays::<Int64Type>(
            vec![None, None, Some(20), Some(30), Some(40), Some(50)],
            index,
            None,
            vec![Some(20), Some(30), None, None],
        )
        .unwrap();
    }

    #[test]
    fn test_take_primitive() {
        let index = UInt32Array::from(vec![Some(3), None, Some(1), Some(3), Some(2)]);

        // int8
        test_take_primitive_arrays::<Int8Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        )
        .unwrap();

        // int16
        test_take_primitive_arrays::<Int16Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        )
        .unwrap();

        // int32
        test_take_primitive_arrays::<Int32Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        )
        .unwrap();

        // int64
        test_take_primitive_arrays::<Int64Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        )
        .unwrap();

        // uint8
        test_take_primitive_arrays::<UInt8Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        )
        .unwrap();

        // uint16
        test_take_primitive_arrays::<UInt16Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        )
        .unwrap();

        // uint32
        test_take_primitive_arrays::<UInt32Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        )
        .unwrap();

        // int64
        test_take_primitive_arrays::<Int64Type>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        )
        .unwrap();

        // interval_year_month
        test_take_primitive_arrays::<IntervalYearMonthType>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        )
        .unwrap();

        // interval_day_time
        test_take_primitive_arrays::<IntervalDayTimeType>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        )
        .unwrap();

        // interval_month_day_nano
        test_take_primitive_arrays::<IntervalMonthDayNanoType>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        )
        .unwrap();

        // duration_second
        test_take_primitive_arrays::<DurationSecondType>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        )
        .unwrap();

        // duration_millisecond
        test_take_primitive_arrays::<DurationMillisecondType>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        )
        .unwrap();

        // duration_microsecond
        test_take_primitive_arrays::<DurationMicrosecondType>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        )
        .unwrap();

        // duration_nanosecond
        test_take_primitive_arrays::<DurationNanosecondType>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        )
        .unwrap();

        // float32
        test_take_primitive_arrays::<Float32Type>(
            vec![Some(0.0), None, Some(2.21), Some(-3.1), None],
            &index,
            None,
            vec![Some(-3.1), None, None, Some(-3.1), Some(2.21)],
        )
        .unwrap();

        // float64
        test_take_primitive_arrays::<Float64Type>(
            vec![Some(0.0), None, Some(2.21), Some(-3.1), None],
            &index,
            None,
            vec![Some(-3.1), None, None, Some(-3.1), Some(2.21)],
        )
        .unwrap();
    }

    #[test]
    fn test_take_preserve_timezone() {
        let index = Int64Array::from(vec![Some(0), None]);

        let input = TimestampNanosecondArray::from_vec(
            vec![1_639_715_368_000_000_000, 1_639_715_368_000_000_000],
            Some("UTC".to_owned()),
        );
        let result = take_impl(&input, &index, None).unwrap();
        match result.data_type() {
            DataType::Timestamp(TimeUnit::Nanosecond, tz) => {
                assert_eq!(tz.clone(), Some("UTC".to_owned()))
            }
            _ => panic!(),
        }
    }

    #[test]
    fn test_take_impl_primitive_with_int64_indices() {
        let index = Int64Array::from(vec![Some(3), None, Some(1), Some(3), Some(2)]);

        // int16
        test_take_impl_primitive_arrays::<Int16Type, Int64Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        );

        // int64
        test_take_impl_primitive_arrays::<Int64Type, Int64Type>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        );

        // uint64
        test_take_impl_primitive_arrays::<UInt64Type, Int64Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        );

        // duration_millisecond
        test_take_impl_primitive_arrays::<DurationMillisecondType, Int64Type>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        );

        // float32
        test_take_impl_primitive_arrays::<Float32Type, Int64Type>(
            vec![Some(0.0), None, Some(2.21), Some(-3.1), None],
            &index,
            None,
            vec![Some(-3.1), None, None, Some(-3.1), Some(2.21)],
        );
    }

    #[test]
    fn test_take_impl_primitive_with_uint8_indices() {
        let index = UInt8Array::from(vec![Some(3), None, Some(1), Some(3), Some(2)]);

        // int16
        test_take_impl_primitive_arrays::<Int16Type, UInt8Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            None,
            vec![Some(3), None, None, Some(3), Some(2)],
        );

        // duration_millisecond
        test_take_impl_primitive_arrays::<DurationMillisecondType, UInt8Type>(
            vec![Some(0), None, Some(2), Some(-15), None],
            &index,
            None,
            vec![Some(-15), None, None, Some(-15), Some(2)],
        );

        // float32
        test_take_impl_primitive_arrays::<Float32Type, UInt8Type>(
            vec![Some(0.0), None, Some(2.21), Some(-3.1), None],
            &index,
            None,
            vec![Some(-3.1), None, None, Some(-3.1), Some(2.21)],
        );
    }

    #[test]
    fn test_take_bool() {
        let index = UInt32Array::from(vec![Some(3), None, Some(1), Some(3), Some(2)]);
        // boolean
        test_take_boolean_arrays(
            vec![Some(false), None, Some(true), Some(false), None],
            &index,
            None,
            vec![Some(false), None, None, Some(false), Some(true)],
        );
    }

    #[test]
    fn test_take_bool_with_offset() {
        let index =
            UInt32Array::from(vec![Some(3), None, Some(1), Some(3), Some(2), None]);
        let index = index.slice(2, 4);
        let index = index
            .as_any()
            .downcast_ref::<PrimitiveArray<UInt32Type>>()
            .unwrap();

        // boolean
        test_take_boolean_arrays(
            vec![Some(false), None, Some(true), Some(false), None],
            index,
            None,
            vec![None, Some(false), Some(true), None],
        );
    }

    fn _test_take_string<'a, K: 'static>()
    where
        K: Array + PartialEq + From<Vec<Option<&'a str>>>,
    {
        let index = UInt32Array::from(vec![Some(3), None, Some(1), Some(3), Some(4)]);

        let array = K::from(vec![
            Some("one"),
            None,
            Some("three"),
            Some("four"),
            Some("five"),
        ]);
        let actual = take(&array, &index, None).unwrap();
        assert_eq!(actual.len(), index.len());

        let actual = actual.as_any().downcast_ref::<K>().unwrap();

        let expected =
            K::from(vec![Some("four"), None, None, Some("four"), Some("five")]);

        assert_eq!(actual, &expected);
    }

    #[test]
    fn test_take_string() {
        _test_take_string::<StringArray>()
    }

    #[test]
    fn test_take_large_string() {
        _test_take_string::<LargeStringArray>()
    }

    macro_rules! test_take_list {
        ($offset_type:ty, $list_data_type:ident, $list_array_type:ident) => {{
            // Construct a value array, [[0,0,0], [-1,-2,-1], [2,3]]
            let value_data = Int32Array::from(vec![0, 0, 0, -1, -2, -1, 2, 3])
                .data()
                .clone();
            // Construct offsets
            let value_offsets: [$offset_type; 4] = [0, 3, 6, 8];
            let value_offsets = Buffer::from_slice_ref(&value_offsets);
            // Construct a list array from the above two
            let list_data_type = DataType::$list_data_type(Box::new(Field::new(
                "item",
                DataType::Int32,
                false,
            )));
            let list_data = ArrayData::builder(list_data_type.clone())
                .len(3)
                .add_buffer(value_offsets)
                .add_child_data(value_data)
                .build()
                .unwrap();
            let list_array = $list_array_type::from(list_data);

            // index returns: [[2,3], null, [-1,-2,-1], [2,3], [0,0,0]]
            let index = UInt32Array::from(vec![Some(2), None, Some(1), Some(2), Some(0)]);

            let a = take(&list_array, &index, None).unwrap();
            let a: &$list_array_type =
                a.as_any().downcast_ref::<$list_array_type>().unwrap();

            // construct a value array with expected results:
            // [[2,3], null, [-1,-2,-1], [2,3], [0,0,0]]
            let expected_data = Int32Array::from(vec![
                Some(2),
                Some(3),
                Some(-1),
                Some(-2),
                Some(-1),
                Some(2),
                Some(3),
                Some(0),
                Some(0),
                Some(0),
            ])
            .data()
            .clone();
            // construct offsets
            let expected_offsets: [$offset_type; 6] = [0, 2, 2, 5, 7, 10];
            let expected_offsets = Buffer::from_slice_ref(&expected_offsets);
            // construct list array from the two
            let expected_list_data = ArrayData::builder(list_data_type)
                .len(5)
                // null buffer remains the same as only the indices have nulls
                .null_bit_buffer(index.data().null_buffer().cloned())
                .add_buffer(expected_offsets)
                .add_child_data(expected_data)
                .build()
                .unwrap();
            let expected_list_array = $list_array_type::from(expected_list_data);

            assert_eq!(a, &expected_list_array);
        }};
    }

    macro_rules! test_take_list_with_value_nulls {
        ($offset_type:ty, $list_data_type:ident, $list_array_type:ident) => {{
            // Construct a value array, [[0,null,0], [-1,-2,3], [null], [5,null]]
            let value_data = Int32Array::from(vec![
                Some(0),
                None,
                Some(0),
                Some(-1),
                Some(-2),
                Some(3),
                None,
                Some(5),
                None,
            ])
            .data()
            .clone();
            // Construct offsets
            let value_offsets: [$offset_type; 5] = [0, 3, 6, 7, 9];
            let value_offsets = Buffer::from_slice_ref(&value_offsets);
            // Construct a list array from the above two
            let list_data_type = DataType::$list_data_type(Box::new(Field::new(
                "item",
                DataType::Int32,
                false,
            )));
            let list_data = ArrayData::builder(list_data_type.clone())
                .len(4)
                .add_buffer(value_offsets)
                .null_bit_buffer(Some(Buffer::from([0b10111101, 0b00000000])))
                .add_child_data(value_data)
                .build()
                .unwrap();
            let list_array = $list_array_type::from(list_data);

            // index returns: [[null], null, [-1,-2,3], [2,null], [0,null,0]]
            let index = UInt32Array::from(vec![Some(2), None, Some(1), Some(3), Some(0)]);

            let a = take(&list_array, &index, None).unwrap();
            let a: &$list_array_type =
                a.as_any().downcast_ref::<$list_array_type>().unwrap();

            // construct a value array with expected results:
            // [[null], null, [-1,-2,3], [5,null], [0,null,0]]
            let expected_data = Int32Array::from(vec![
                None,
                Some(-1),
                Some(-2),
                Some(3),
                Some(5),
                None,
                Some(0),
                None,
                Some(0),
            ])
            .data()
            .clone();
            // construct offsets
            let expected_offsets: [$offset_type; 6] = [0, 1, 1, 4, 6, 9];
            let expected_offsets = Buffer::from_slice_ref(&expected_offsets);
            // construct list array from the two
            let expected_list_data = ArrayData::builder(list_data_type)
                .len(5)
                // null buffer remains the same as only the indices have nulls
                .null_bit_buffer(index.data().null_buffer().cloned())
                .add_buffer(expected_offsets)
                .add_child_data(expected_data)
                .build()
                .unwrap();
            let expected_list_array = $list_array_type::from(expected_list_data);

            assert_eq!(a, &expected_list_array);
        }};
    }

    macro_rules! test_take_list_with_nulls {
        ($offset_type:ty, $list_data_type:ident, $list_array_type:ident) => {{
            // Construct a value array, [[0,null,0], [-1,-2,3], null, [5,null]]
            let value_data = Int32Array::from(vec![
                Some(0),
                None,
                Some(0),
                Some(-1),
                Some(-2),
                Some(3),
                Some(5),
                None,
            ])
            .data()
            .clone();
            // Construct offsets
            let value_offsets: [$offset_type; 5] = [0, 3, 6, 6, 8];
            let value_offsets = Buffer::from_slice_ref(&value_offsets);
            // Construct a list array from the above two
            let list_data_type = DataType::$list_data_type(Box::new(Field::new(
                "item",
                DataType::Int32,
                false,
            )));
            let list_data = ArrayData::builder(list_data_type.clone())
                .len(4)
                .add_buffer(value_offsets)
                .null_bit_buffer(Some(Buffer::from([0b01111101])))
                .add_child_data(value_data)
                .build()
                .unwrap();
            let list_array = $list_array_type::from(list_data);

            // index returns: [null, null, [-1,-2,3], [5,null], [0,null,0]]
            let index = UInt32Array::from(vec![Some(2), None, Some(1), Some(3), Some(0)]);

            let a = take(&list_array, &index, None).unwrap();
            let a: &$list_array_type =
                a.as_any().downcast_ref::<$list_array_type>().unwrap();

            // construct a value array with expected results:
            // [null, null, [-1,-2,3], [5,null], [0,null,0]]
            let expected_data = Int32Array::from(vec![
                Some(-1),
                Some(-2),
                Some(3),
                Some(5),
                None,
                Some(0),
                None,
                Some(0),
            ])
            .data()
            .clone();
            // construct offsets
            let expected_offsets: [$offset_type; 6] = [0, 0, 0, 3, 5, 8];
            let expected_offsets = Buffer::from_slice_ref(&expected_offsets);
            // construct list array from the two
            let mut null_bits: [u8; 1] = [0; 1];
            bit_util::set_bit(&mut null_bits, 2);
            bit_util::set_bit(&mut null_bits, 3);
            bit_util::set_bit(&mut null_bits, 4);
            let expected_list_data = ArrayData::builder(list_data_type)
                .len(5)
                // null buffer must be recalculated as both values and indices have nulls
                .null_bit_buffer(Some(Buffer::from(null_bits)))
                .add_buffer(expected_offsets)
                .add_child_data(expected_data)
                .build()
                .unwrap();
            let expected_list_array = $list_array_type::from(expected_list_data);

            assert_eq!(a, &expected_list_array);
        }};
    }

    fn do_take_fixed_size_list_test<T>(
        length: <Int32Type as ArrowPrimitiveType>::Native,
        input_data: Vec<Option<Vec<Option<T::Native>>>>,
        indices: Vec<<UInt32Type as ArrowPrimitiveType>::Native>,
        expected_data: Vec<Option<Vec<Option<T::Native>>>>,
    ) where
        T: ArrowPrimitiveType,
        PrimitiveArray<T>: From<Vec<Option<T::Native>>>,
    {
        let indices = UInt32Array::from(indices);

        let input_array = build_fixed_size_list_nullable::<T>(input_data, length);

        let output = take_fixed_size_list(&input_array, &indices, length as u32).unwrap();

        let expected = build_fixed_size_list_nullable::<T>(expected_data, length);

        assert_eq!(&output, &expected)
    }

    #[test]
    fn test_take_list() {
        test_take_list!(i32, List, ListArray);
    }

    #[test]
    fn test_take_large_list() {
        test_take_list!(i64, LargeList, LargeListArray);
    }

    #[test]
    fn test_take_list_with_value_nulls() {
        test_take_list_with_value_nulls!(i32, List, ListArray);
    }

    #[test]
    fn test_take_large_list_with_value_nulls() {
        test_take_list_with_value_nulls!(i64, LargeList, LargeListArray);
    }

    #[test]
    fn test_test_take_list_with_nulls() {
        test_take_list_with_nulls!(i32, List, ListArray);
    }

    #[test]
    fn test_test_take_large_list_with_nulls() {
        test_take_list_with_nulls!(i64, LargeList, LargeListArray);
    }

    #[test]
    fn test_take_fixed_size_list() {
        do_take_fixed_size_list_test::<Int32Type>(
            3,
            vec![
                Some(vec![None, Some(1), Some(2)]),
                Some(vec![Some(3), Some(4), None]),
                Some(vec![Some(6), Some(7), Some(8)]),
            ],
            vec![2, 1, 0],
            vec![
                Some(vec![Some(6), Some(7), Some(8)]),
                Some(vec![Some(3), Some(4), None]),
                Some(vec![None, Some(1), Some(2)]),
            ],
        );

        do_take_fixed_size_list_test::<UInt8Type>(
            1,
            vec![
                Some(vec![Some(1)]),
                Some(vec![Some(2)]),
                Some(vec![Some(3)]),
                Some(vec![Some(4)]),
                Some(vec![Some(5)]),
                Some(vec![Some(6)]),
                Some(vec![Some(7)]),
                Some(vec![Some(8)]),
            ],
            vec![2, 7, 0],
            vec![
                Some(vec![Some(3)]),
                Some(vec![Some(8)]),
                Some(vec![Some(1)]),
            ],
        );

        do_take_fixed_size_list_test::<UInt64Type>(
            3,
            vec![
                Some(vec![Some(10), Some(11), Some(12)]),
                Some(vec![Some(13), Some(14), Some(15)]),
                None,
                Some(vec![Some(16), Some(17), Some(18)]),
            ],
            vec![3, 2, 1, 2, 0],
            vec![
                Some(vec![Some(16), Some(17), Some(18)]),
                None,
                Some(vec![Some(13), Some(14), Some(15)]),
                None,
                Some(vec![Some(10), Some(11), Some(12)]),
            ],
        );
    }

    #[test]
    #[should_panic(expected = "index out of bounds: the len is 4 but the index is 1000")]
    fn test_take_list_out_of_bounds() {
        // Construct a value array, [[0,0,0], [-1,-2,-1], [2,3]]
        let value_data = Int32Array::from(vec![0, 0, 0, -1, -2, -1, 2, 3])
            .data()
            .clone();
        // Construct offsets
        let value_offsets = Buffer::from_slice_ref(&[0, 3, 6, 8]);
        // Construct a list array from the above two
        let list_data_type =
            DataType::List(Box::new(Field::new("item", DataType::Int32, false)));
        let list_data = ArrayData::builder(list_data_type)
            .len(3)
            .add_buffer(value_offsets)
            .add_child_data(value_data)
            .build()
            .unwrap();
        let list_array = ListArray::from(list_data);

        let index = UInt32Array::from(vec![1000]);

        // A panic is expected here since we have not supplied the check_bounds
        // option.
        take(&list_array, &index, None).unwrap();
    }

    #[test]
    fn test_take_struct() {
        let array = create_test_struct(vec![
            Some((Some(true), Some(42))),
            Some((Some(false), Some(28))),
            Some((Some(false), Some(19))),
            Some((Some(true), Some(31))),
            None,
        ]);

        let index = UInt32Array::from(vec![0, 3, 1, 0, 2, 4]);
        let actual = take(&array, &index, None).unwrap();
        let actual: &StructArray = actual.as_any().downcast_ref::<StructArray>().unwrap();
        assert_eq!(index.len(), actual.len());
        assert_eq!(1, actual.null_count());

        let expected = create_test_struct(vec![
            Some((Some(true), Some(42))),
            Some((Some(true), Some(31))),
            Some((Some(false), Some(28))),
            Some((Some(true), Some(42))),
            Some((Some(false), Some(19))),
            None,
        ]);

        assert_eq!(&expected, actual);
    }

    #[test]
    fn test_take_struct_with_null_indices() {
        let array = create_test_struct(vec![
            Some((Some(true), Some(42))),
            Some((Some(false), Some(28))),
            Some((Some(false), Some(19))),
            Some((Some(true), Some(31))),
            None,
        ]);

        let index =
            UInt32Array::from(vec![None, Some(3), Some(1), None, Some(0), Some(4)]);
        let actual = take(&array, &index, None).unwrap();
        let actual: &StructArray = actual.as_any().downcast_ref::<StructArray>().unwrap();
        assert_eq!(index.len(), actual.len());
        assert_eq!(3, actual.null_count()); // 2 because of indices, 1 because of struct array

        let expected = create_test_struct(vec![
            None,
            Some((Some(true), Some(31))),
            Some((Some(false), Some(28))),
            None,
            Some((Some(true), Some(42))),
            None,
        ]);

        assert_eq!(&expected, actual);
    }

    #[test]
    fn test_take_out_of_bounds() {
        let index = UInt32Array::from(vec![Some(3), None, Some(1), Some(3), Some(6)]);
        let take_opt = TakeOptions { check_bounds: true };

        // int64
        let result = test_take_primitive_arrays::<Int64Type>(
            vec![Some(0), None, Some(2), Some(3), None],
            &index,
            Some(take_opt),
            vec![None],
        );
        assert!(result.is_err());
    }

    #[test]
    #[should_panic(expected = "index out of bounds: the len is 4 but the index is 1000")]
    fn test_take_out_of_bounds_panic() {
        let index = UInt32Array::from(vec![Some(1000)]);

        test_take_primitive_arrays::<Int64Type>(
            vec![Some(0), Some(1), Some(2), Some(3)],
            &index,
            None,
            vec![None],
        )
        .unwrap();
    }

    #[test]
    fn test_null_array_smaller_than_indices() {
        let values = NullArray::new(2);
        let indices = UInt32Array::from(vec![Some(0), None, Some(15)]);

        let result = take(&values, &indices, None).unwrap();
        let expected: ArrayRef = Arc::new(NullArray::new(3));
        assert_eq!(&result, &expected);
    }

    #[test]
    fn test_null_array_larger_than_indices() {
        let values = NullArray::new(5);
        let indices = UInt32Array::from(vec![Some(0), None, Some(15)]);

        let result = take(&values, &indices, None).unwrap();
        let expected: ArrayRef = Arc::new(NullArray::new(3));
        assert_eq!(&result, &expected);
    }

    #[test]
    fn test_null_array_indices_out_of_bounds() {
        let values = NullArray::new(5);
        let indices = UInt32Array::from(vec![Some(0), None, Some(15)]);

        let result = take(&values, &indices, Some(TakeOptions { check_bounds: true }));
        assert_eq!(
            result.unwrap_err().to_string(),
            "Compute error: Array index out of bounds, cannot get item at index 15 from 5 entries"
        );
    }

    #[test]
    fn test_take_dict() {
        let keys_builder = Int16Builder::new(8);
        let values_builder = StringBuilder::new(4);

        let mut dict_builder = StringDictionaryBuilder::new(keys_builder, values_builder);

        dict_builder.append("foo").unwrap();
        dict_builder.append("bar").unwrap();
        dict_builder.append("").unwrap();
        dict_builder.append_null().unwrap();
        dict_builder.append("foo").unwrap();
        dict_builder.append("bar").unwrap();
        dict_builder.append("bar").unwrap();
        dict_builder.append("foo").unwrap();

        let array = dict_builder.finish();
        let dict_values = array.values().clone();
        let dict_values = dict_values.as_any().downcast_ref::<StringArray>().unwrap();

        let indices = UInt32Array::from(vec![
            Some(0), // first "foo"
            Some(7), // last "foo"
            None,    // null index should return null
            Some(5), // second "bar"
            Some(6), // another "bar"
            Some(2), // empty string
            Some(3), // input is null at this index
        ]);

        let result = take(&array, &indices, None).unwrap();
        let result = result
            .as_any()
            .downcast_ref::<DictionaryArray<Int16Type>>()
            .unwrap();

        let result_values: StringArray = result.values().data().clone().into();

        // dictionary values should stay the same
        let expected_values = StringArray::from(vec!["foo", "bar", ""]);
        assert_eq!(&expected_values, dict_values);
        assert_eq!(&expected_values, &result_values);

        let expected_keys = Int16Array::from(vec![
            Some(0),
            Some(0),
            None,
            Some(1),
            Some(1),
            Some(2),
            None,
        ]);
        assert_eq!(result.keys(), &expected_keys);
    }
}
