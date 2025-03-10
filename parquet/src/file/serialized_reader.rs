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

//! Contains implementations of the reader traits FileReader, RowGroupReader and PageReader
//! Also contains implementations of the ChunkReader for files (with buffering) and byte arrays (RAM)

use bytes::{Buf, Bytes};
use std::{convert::TryFrom, fs::File, io::Read, path::Path, sync::Arc};

use parquet_format::{PageHeader, PageType};
use thrift::protocol::TCompactInputProtocol;

use crate::basic::{Compression, Encoding, Type};
use crate::column::page::{Page, PageReader};
use crate::compression::{create_codec, Codec};
use crate::errors::{ParquetError, Result};
use crate::file::page_index::index_reader;
use crate::file::{footer, metadata::*, reader::*, statistics};
use crate::record::reader::RowIter;
use crate::record::Row;
use crate::schema::types::Type as SchemaType;
use crate::util::{io::TryClone, memory::ByteBufferPtr};

// export `SliceableCursor` and `FileSource` publically so clients can
// re-use the logic in their own ParquetFileWriter wrappers
#[allow(deprecated)]
pub use crate::util::{cursor::SliceableCursor, io::FileSource};

// ----------------------------------------------------------------------
// Implementations of traits facilitating the creation of a new reader

impl Length for File {
    fn len(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0u64)
    }
}

impl TryClone for File {
    fn try_clone(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
}

impl ChunkReader for File {
    type T = FileSource<File>;

    fn get_read(&self, start: u64, length: usize) -> Result<Self::T> {
        Ok(FileSource::new(self, start, length))
    }
}

impl Length for Bytes {
    fn len(&self) -> u64 {
        self.len() as u64
    }
}

impl TryClone for Bytes {
    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(self.clone())
    }
}

impl ChunkReader for Bytes {
    type T = bytes::buf::Reader<Bytes>;

    fn get_read(&self, start: u64, length: usize) -> Result<Self::T> {
        let start = start as usize;
        Ok(self.slice(start..start + length).reader())
    }
}

#[allow(deprecated)]
impl Length for SliceableCursor {
    fn len(&self) -> u64 {
        SliceableCursor::len(self)
    }
}

#[allow(deprecated)]
impl ChunkReader for SliceableCursor {
    type T = SliceableCursor;

    fn get_read(&self, start: u64, length: usize) -> Result<Self::T> {
        self.slice(start, length).map_err(|e| e.into())
    }
}

impl TryFrom<File> for SerializedFileReader<File> {
    type Error = ParquetError;

    fn try_from(file: File) -> Result<Self> {
        Self::new(file)
    }
}

impl<'a> TryFrom<&'a Path> for SerializedFileReader<File> {
    type Error = ParquetError;

    fn try_from(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        Self::try_from(file)
    }
}

impl TryFrom<String> for SerializedFileReader<File> {
    type Error = ParquetError;

    fn try_from(path: String) -> Result<Self> {
        Self::try_from(Path::new(&path))
    }
}

impl<'a> TryFrom<&'a str> for SerializedFileReader<File> {
    type Error = ParquetError;

    fn try_from(path: &str) -> Result<Self> {
        Self::try_from(Path::new(&path))
    }
}

/// Conversion into a [`RowIter`](crate::record::reader::RowIter)
/// using the full file schema over all row groups.
impl IntoIterator for SerializedFileReader<File> {
    type Item = Row;
    type IntoIter = RowIter<'static>;

    fn into_iter(self) -> Self::IntoIter {
        RowIter::from_file_into(Box::new(self))
    }
}

// ----------------------------------------------------------------------
// Implementations of file & row group readers

/// A serialized implementation for Parquet [`FileReader`].
pub struct SerializedFileReader<R: ChunkReader> {
    chunk_reader: Arc<R>,
    metadata: ParquetMetaData,
}

/// A builder for [`ReadOptions`].
/// For the predicates that are added to the builder,
/// they will be chained using 'AND' to filter the row groups.
pub struct ReadOptionsBuilder {
    predicates: Vec<Box<dyn FnMut(&RowGroupMetaData, usize) -> bool>>,
    enable_page_index: bool,
}

impl ReadOptionsBuilder {
    /// New builder
    pub fn new() -> Self {
        ReadOptionsBuilder {
            predicates: vec![],
            enable_page_index: false,
        }
    }

    /// Add a predicate on row group metadata to the reading option,
    /// Filter only row groups that match the predicate criteria
    pub fn with_predicate(
        mut self,
        predicate: Box<dyn FnMut(&RowGroupMetaData, usize) -> bool>,
    ) -> Self {
        self.predicates.push(predicate);
        self
    }

    /// Add a range predicate on filtering row groups if their midpoints are within
    /// the Closed-Open range `[start..end) {x | start <= x < end}`
    pub fn with_range(mut self, start: i64, end: i64) -> Self {
        assert!(start < end);
        let predicate = move |rg: &RowGroupMetaData, _: usize| {
            let mid = get_midpoint_offset(rg);
            mid >= start && mid < end
        };
        self.predicates.push(Box::new(predicate));
        self
    }

    /// Enable page index in the reading option,
    pub fn with_page_index(mut self) -> Self {
        self.enable_page_index = true;
        self
    }

    /// Seal the builder and return the read options
    pub fn build(self) -> ReadOptions {
        ReadOptions {
            predicates: self.predicates,
            enable_page_index: self.enable_page_index,
        }
    }
}

/// A collection of options for reading a Parquet file.
///
/// Currently, only predicates on row group metadata are supported.
/// All predicates will be chained using 'AND' to filter the row groups.
pub struct ReadOptions {
    predicates: Vec<Box<dyn FnMut(&RowGroupMetaData, usize) -> bool>>,
    enable_page_index: bool,
}

impl<R: 'static + ChunkReader> SerializedFileReader<R> {
    /// Creates file reader from a Parquet file.
    /// Returns error if Parquet file does not exist or is corrupt.
    pub fn new(chunk_reader: R) -> Result<Self> {
        let metadata = footer::parse_metadata(&chunk_reader)?;
        Ok(Self {
            chunk_reader: Arc::new(chunk_reader),
            metadata,
        })
    }

    /// Creates file reader from a Parquet file with read options.
    /// Returns error if Parquet file does not exist or is corrupt.
    pub fn new_with_options(chunk_reader: R, options: ReadOptions) -> Result<Self> {
        let metadata = footer::parse_metadata(&chunk_reader)?;
        let mut predicates = options.predicates;
        let row_groups = metadata.row_groups().to_vec();
        let mut filtered_row_groups = Vec::<RowGroupMetaData>::new();
        for (i, rg_meta) in row_groups.into_iter().enumerate() {
            let mut keep = true;
            for predicate in &mut predicates {
                if !predicate(&rg_meta, i) {
                    keep = false;
                    break;
                }
            }
            if keep {
                filtered_row_groups.push(rg_meta);
            }
        }

        if options.enable_page_index {
            //Todo for now test data `data_index_bloom_encoding_stats.parquet` only have one rowgroup
            //support multi after create multi-RG test data.
            let cols = metadata.row_group(0);
            let columns_indexes =
                index_reader::read_columns_indexes(&chunk_reader, cols.columns())?;
            let pages_locations =
                index_reader::read_pages_locations(&chunk_reader, cols.columns())?;

            Ok(Self {
                chunk_reader: Arc::new(chunk_reader),
                metadata: ParquetMetaData::new_with_page_index(
                    metadata.file_metadata().clone(),
                    filtered_row_groups,
                    Some(columns_indexes),
                    Some(pages_locations),
                ),
            })
        } else {
            Ok(Self {
                chunk_reader: Arc::new(chunk_reader),
                metadata: ParquetMetaData::new(
                    metadata.file_metadata().clone(),
                    filtered_row_groups,
                ),
            })
        }
    }
}

/// Get midpoint offset for a row group
fn get_midpoint_offset(meta: &RowGroupMetaData) -> i64 {
    let col = meta.column(0);
    let mut offset = col.data_page_offset();
    if let Some(dic_offset) = col.dictionary_page_offset() {
        if offset > dic_offset {
            offset = dic_offset
        }
    };
    offset + meta.compressed_size() / 2
}

impl<R: 'static + ChunkReader> FileReader for SerializedFileReader<R> {
    fn metadata(&self) -> &ParquetMetaData {
        &self.metadata
    }

    fn num_row_groups(&self) -> usize {
        self.metadata.num_row_groups()
    }

    fn get_row_group(&self, i: usize) -> Result<Box<dyn RowGroupReader + '_>> {
        let row_group_metadata = self.metadata.row_group(i);
        // Row groups should be processed sequentially.
        let f = Arc::clone(&self.chunk_reader);
        Ok(Box::new(SerializedRowGroupReader::new(
            f,
            row_group_metadata,
        )))
    }

    fn get_row_iter(&self, projection: Option<SchemaType>) -> Result<RowIter> {
        RowIter::from_file(projection, self)
    }
}

/// A serialized implementation for Parquet [`RowGroupReader`].
pub struct SerializedRowGroupReader<'a, R: ChunkReader> {
    chunk_reader: Arc<R>,
    metadata: &'a RowGroupMetaData,
}

impl<'a, R: ChunkReader> SerializedRowGroupReader<'a, R> {
    /// Creates new row group reader from a file and row group metadata.
    fn new(chunk_reader: Arc<R>, metadata: &'a RowGroupMetaData) -> Self {
        Self {
            chunk_reader,
            metadata,
        }
    }
}

impl<'a, R: 'static + ChunkReader> RowGroupReader for SerializedRowGroupReader<'a, R> {
    fn metadata(&self) -> &RowGroupMetaData {
        self.metadata
    }

    fn num_columns(&self) -> usize {
        self.metadata.num_columns()
    }

    // TODO: fix PARQUET-816
    fn get_column_page_reader(&self, i: usize) -> Result<Box<dyn PageReader>> {
        let col = self.metadata.column(i);
        let (col_start, col_length) = col.byte_range();
        //Todo filter with multi row range
        let file_chunk = self.chunk_reader.get_read(col_start, col_length as usize)?;
        let page_reader = SerializedPageReader::new(
            file_chunk,
            col.num_values(),
            col.compression(),
            col.column_descr().physical_type(),
        )?;
        Ok(Box::new(page_reader))
    }

    fn get_row_iter(&self, projection: Option<SchemaType>) -> Result<RowIter> {
        RowIter::from_row_group(projection, self)
    }
}

/// Reads a [`PageHeader`] from the provided [`Read`]
pub(crate) fn read_page_header<T: Read>(input: &mut T) -> Result<PageHeader> {
    let mut prot = TCompactInputProtocol::new(input);
    let page_header = PageHeader::read_from_in_protocol(&mut prot)?;
    Ok(page_header)
}

/// Decodes a [`Page`] from the provided `buffer`
pub(crate) fn decode_page(
    page_header: PageHeader,
    buffer: ByteBufferPtr,
    physical_type: Type,
    decompressor: Option<&mut Box<dyn Codec>>,
) -> Result<Page> {
    // When processing data page v2, depending on enabled compression for the
    // page, we should account for uncompressed data ('offset') of
    // repetition and definition levels.
    //
    // We always use 0 offset for other pages other than v2, `true` flag means
    // that compression will be applied if decompressor is defined
    let mut offset: usize = 0;
    let mut can_decompress = true;

    if let Some(ref header_v2) = page_header.data_page_header_v2 {
        offset = (header_v2.definition_levels_byte_length
            + header_v2.repetition_levels_byte_length) as usize;
        // When is_compressed flag is missing the page is considered compressed
        can_decompress = header_v2.is_compressed.unwrap_or(true);
    }

    // TODO: page header could be huge because of statistics. We should set a
    // maximum page header size and abort if that is exceeded.
    let buffer = match decompressor {
        Some(decompressor) if can_decompress => {
            let uncompressed_size = page_header.uncompressed_page_size as usize;
            let mut decompressed = Vec::with_capacity(uncompressed_size);
            let compressed = &buffer.as_ref()[offset..];
            decompressed.extend_from_slice(&buffer.as_ref()[..offset]);
            decompressor.decompress(compressed, &mut decompressed)?;

            if decompressed.len() != uncompressed_size {
                return Err(general_err!(
                    "Actual decompressed size doesn't match the expected one ({} vs {})",
                    decompressed.len(),
                    uncompressed_size
                ));
            }

            ByteBufferPtr::new(decompressed)
        }
        _ => buffer,
    };

    let result = match page_header.type_ {
        PageType::DictionaryPage => {
            assert!(page_header.dictionary_page_header.is_some());
            let dict_header = page_header.dictionary_page_header.as_ref().unwrap();
            let is_sorted = dict_header.is_sorted.unwrap_or(false);
            Page::DictionaryPage {
                buf: buffer,
                num_values: dict_header.num_values as u32,
                encoding: Encoding::from(dict_header.encoding),
                is_sorted,
            }
        }
        PageType::DataPage => {
            assert!(page_header.data_page_header.is_some());
            let header = page_header.data_page_header.unwrap();
            Page::DataPage {
                buf: buffer,
                num_values: header.num_values as u32,
                encoding: Encoding::from(header.encoding),
                def_level_encoding: Encoding::from(header.definition_level_encoding),
                rep_level_encoding: Encoding::from(header.repetition_level_encoding),
                statistics: statistics::from_thrift(physical_type, header.statistics),
            }
        }
        PageType::DataPageV2 => {
            assert!(page_header.data_page_header_v2.is_some());
            let header = page_header.data_page_header_v2.unwrap();
            let is_compressed = header.is_compressed.unwrap_or(true);
            Page::DataPageV2 {
                buf: buffer,
                num_values: header.num_values as u32,
                encoding: Encoding::from(header.encoding),
                num_nulls: header.num_nulls as u32,
                num_rows: header.num_rows as u32,
                def_levels_byte_len: header.definition_levels_byte_length as u32,
                rep_levels_byte_len: header.repetition_levels_byte_length as u32,
                is_compressed,
                statistics: statistics::from_thrift(physical_type, header.statistics),
            }
        }
        _ => {
            // For unknown page type (e.g., INDEX_PAGE), skip and read next.
            unimplemented!("Page type {:?} is not supported", page_header.type_)
        }
    };

    Ok(result)
}

/// A serialized implementation for Parquet [`PageReader`].
pub struct SerializedPageReader<T: Read> {
    // The file source buffer which references exactly the bytes for the column trunk
    // to be read by this page reader.
    buf: T,

    // The compression codec for this column chunk. Only set for non-PLAIN codec.
    decompressor: Option<Box<dyn Codec>>,

    // The number of values we have seen so far.
    seen_num_values: i64,

    // The number of total values in this column chunk.
    total_num_values: i64,

    // Column chunk type.
    physical_type: Type,
}

impl<T: Read> SerializedPageReader<T> {
    /// Creates a new serialized page reader from file source.
    pub fn new(
        buf: T,
        total_num_values: i64,
        compression: Compression,
        physical_type: Type,
    ) -> Result<Self> {
        let decompressor = create_codec(compression)?;
        let result = Self {
            buf,
            total_num_values,
            seen_num_values: 0,
            decompressor,
            physical_type,
        };
        Ok(result)
    }
}

impl<T: Read + Send> Iterator for SerializedPageReader<T> {
    type Item = Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        self.get_next_page().transpose()
    }
}

impl<T: Read + Send> PageReader for SerializedPageReader<T> {
    fn get_next_page(&mut self) -> Result<Option<Page>> {
        while self.seen_num_values < self.total_num_values {
            let page_header = read_page_header(&mut self.buf)?;

            let to_read = page_header.compressed_page_size as usize;
            let mut buffer = Vec::with_capacity(to_read);
            let read = (&mut self.buf)
                .take(to_read as u64)
                .read_to_end(&mut buffer)?;

            if read != to_read {
                return Err(eof_err!(
                    "Expected to read {} bytes of page, read only {}",
                    to_read,
                    read
                ));
            }

            let buffer = ByteBufferPtr::new(buffer);
            let result = match page_header.type_ {
                PageType::DataPage | PageType::DataPageV2 => {
                    let decoded = decode_page(
                        page_header,
                        buffer,
                        self.physical_type,
                        self.decompressor.as_mut(),
                    )?;
                    self.seen_num_values += decoded.num_values() as i64;
                    decoded
                }
                PageType::DictionaryPage => decode_page(
                    page_header,
                    buffer,
                    self.physical_type,
                    self.decompressor.as_mut(),
                )?,
                _ => {
                    // For unknown page type (e.g., INDEX_PAGE), skip and read next.
                    continue;
                }
            };
            return Ok(Some(result));
        }

        // We are at the end of this column chunk and no more page left. Return None.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::basic::{self, ColumnOrder};
    use crate::file::page_index::index::Index;
    use crate::record::RowAccessor;
    use crate::schema::parser::parse_message_type;
    use crate::util::test_common::{get_test_file, get_test_path};
    use parquet_format::BoundaryOrder;
    use std::sync::Arc;

    #[test]
    fn test_cursor_and_file_has_the_same_behaviour() {
        let mut buf: Vec<u8> = Vec::new();
        get_test_file("alltypes_plain.parquet")
            .read_to_end(&mut buf)
            .unwrap();
        let cursor = Bytes::from(buf);
        let read_from_cursor = SerializedFileReader::new(cursor).unwrap();

        let test_file = get_test_file("alltypes_plain.parquet");
        let read_from_file = SerializedFileReader::new(test_file).unwrap();

        let file_iter = read_from_file.get_row_iter(None).unwrap();
        let cursor_iter = read_from_cursor.get_row_iter(None).unwrap();

        assert!(file_iter.eq(cursor_iter));
    }

    #[test]
    fn test_file_reader_try_from() {
        // Valid file path
        let test_file = get_test_file("alltypes_plain.parquet");
        let test_path_buf = get_test_path("alltypes_plain.parquet");
        let test_path = test_path_buf.as_path();
        let test_path_str = test_path.to_str().unwrap();

        let reader = SerializedFileReader::try_from(test_file);
        assert!(reader.is_ok());

        let reader = SerializedFileReader::try_from(test_path);
        assert!(reader.is_ok());

        let reader = SerializedFileReader::try_from(test_path_str);
        assert!(reader.is_ok());

        let reader = SerializedFileReader::try_from(test_path_str.to_string());
        assert!(reader.is_ok());

        // Invalid file path
        let test_path = Path::new("invalid.parquet");
        let test_path_str = test_path.to_str().unwrap();

        let reader = SerializedFileReader::try_from(test_path);
        assert!(reader.is_err());

        let reader = SerializedFileReader::try_from(test_path_str);
        assert!(reader.is_err());

        let reader = SerializedFileReader::try_from(test_path_str.to_string());
        assert!(reader.is_err());
    }

    #[test]
    fn test_file_reader_into_iter() {
        let path = get_test_path("alltypes_plain.parquet");
        let vec = vec![path.clone(), path]
            .iter()
            .map(|p| SerializedFileReader::try_from(p.as_path()).unwrap())
            .flat_map(|r| r.into_iter())
            .flat_map(|r| r.get_int(0))
            .collect::<Vec<_>>();

        // rows in the parquet file are not sorted by "id"
        // each file contains [id:4, id:5, id:6, id:7, id:2, id:3, id:0, id:1]
        assert_eq!(vec, vec![4, 5, 6, 7, 2, 3, 0, 1, 4, 5, 6, 7, 2, 3, 0, 1]);
    }

    #[test]
    fn test_file_reader_into_iter_project() {
        let path = get_test_path("alltypes_plain.parquet");
        let result = vec![path]
            .iter()
            .map(|p| SerializedFileReader::try_from(p.as_path()).unwrap())
            .flat_map(|r| {
                let schema = "message schema { OPTIONAL INT32 id; }";
                let proj = parse_message_type(schema).ok();

                r.into_iter().project(proj).unwrap()
            })
            .map(|r| format!("{}", r))
            .collect::<Vec<_>>()
            .join(",");

        assert_eq!(
            result,
            "{id: 4},{id: 5},{id: 6},{id: 7},{id: 2},{id: 3},{id: 0},{id: 1}"
        );
    }

    #[test]
    fn test_reuse_file_chunk() {
        // This test covers the case of maintaining the correct start position in a file
        // stream for each column reader after initializing and moving to the next one
        // (without necessarily reading the entire column).
        let test_file = get_test_file("alltypes_plain.parquet");
        let reader = SerializedFileReader::new(test_file).unwrap();
        let row_group = reader.get_row_group(0).unwrap();

        let mut page_readers = Vec::new();
        for i in 0..row_group.num_columns() {
            page_readers.push(row_group.get_column_page_reader(i).unwrap());
        }

        // Now buffer each col reader, we do not expect any failures like:
        // General("underlying Thrift error: end of file")
        for mut page_reader in page_readers {
            assert!(page_reader.get_next_page().is_ok());
        }
    }

    #[test]
    fn test_file_reader() {
        let test_file = get_test_file("alltypes_plain.parquet");
        let reader_result = SerializedFileReader::new(test_file);
        assert!(reader_result.is_ok());
        let reader = reader_result.unwrap();

        // Test contents in Parquet metadata
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);

        // Test contents in file metadata
        let file_metadata = metadata.file_metadata();
        assert!(file_metadata.created_by().is_some());
        assert_eq!(
            file_metadata.created_by().unwrap(),
            "impala version 1.3.0-INTERNAL (build 8a48ddb1eff84592b3fc06bc6f51ec120e1fffc9)"
        );
        assert!(file_metadata.key_value_metadata().is_none());
        assert_eq!(file_metadata.num_rows(), 8);
        assert_eq!(file_metadata.version(), 1);
        assert_eq!(file_metadata.column_orders(), None);

        // Test contents in row group metadata
        let row_group_metadata = metadata.row_group(0);
        assert_eq!(row_group_metadata.num_columns(), 11);
        assert_eq!(row_group_metadata.num_rows(), 8);
        assert_eq!(row_group_metadata.total_byte_size(), 671);
        // Check each column order
        for i in 0..row_group_metadata.num_columns() {
            assert_eq!(file_metadata.column_order(i), ColumnOrder::UNDEFINED);
        }

        // Test row group reader
        let row_group_reader_result = reader.get_row_group(0);
        assert!(row_group_reader_result.is_ok());
        let row_group_reader: Box<dyn RowGroupReader> = row_group_reader_result.unwrap();
        assert_eq!(
            row_group_reader.num_columns(),
            row_group_metadata.num_columns()
        );
        assert_eq!(
            row_group_reader.metadata().total_byte_size(),
            row_group_metadata.total_byte_size()
        );

        // Test page readers
        // TODO: test for every column
        let page_reader_0_result = row_group_reader.get_column_page_reader(0);
        assert!(page_reader_0_result.is_ok());
        let mut page_reader_0: Box<dyn PageReader> = page_reader_0_result.unwrap();
        let mut page_count = 0;
        while let Ok(Some(page)) = page_reader_0.get_next_page() {
            let is_expected_page = match page {
                Page::DictionaryPage {
                    buf,
                    num_values,
                    encoding,
                    is_sorted,
                } => {
                    assert_eq!(buf.len(), 32);
                    assert_eq!(num_values, 8);
                    assert_eq!(encoding, Encoding::PLAIN_DICTIONARY);
                    assert!(!is_sorted);
                    true
                }
                Page::DataPage {
                    buf,
                    num_values,
                    encoding,
                    def_level_encoding,
                    rep_level_encoding,
                    statistics,
                } => {
                    assert_eq!(buf.len(), 11);
                    assert_eq!(num_values, 8);
                    assert_eq!(encoding, Encoding::PLAIN_DICTIONARY);
                    assert_eq!(def_level_encoding, Encoding::RLE);
                    assert_eq!(rep_level_encoding, Encoding::BIT_PACKED);
                    assert!(statistics.is_none());
                    true
                }
                _ => false,
            };
            assert!(is_expected_page);
            page_count += 1;
        }
        assert_eq!(page_count, 2);
    }

    #[test]
    fn test_file_reader_datapage_v2() {
        let test_file = get_test_file("datapage_v2.snappy.parquet");
        let reader_result = SerializedFileReader::new(test_file);
        assert!(reader_result.is_ok());
        let reader = reader_result.unwrap();

        // Test contents in Parquet metadata
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);

        // Test contents in file metadata
        let file_metadata = metadata.file_metadata();
        assert!(file_metadata.created_by().is_some());
        assert_eq!(
            file_metadata.created_by().unwrap(),
            "parquet-mr version 1.8.1 (build 4aba4dae7bb0d4edbcf7923ae1339f28fd3f7fcf)"
        );
        assert!(file_metadata.key_value_metadata().is_some());
        assert_eq!(
            file_metadata.key_value_metadata().to_owned().unwrap().len(),
            1
        );

        assert_eq!(file_metadata.num_rows(), 5);
        assert_eq!(file_metadata.version(), 1);
        assert_eq!(file_metadata.column_orders(), None);

        let row_group_metadata = metadata.row_group(0);

        // Check each column order
        for i in 0..row_group_metadata.num_columns() {
            assert_eq!(file_metadata.column_order(i), ColumnOrder::UNDEFINED);
        }

        // Test row group reader
        let row_group_reader_result = reader.get_row_group(0);
        assert!(row_group_reader_result.is_ok());
        let row_group_reader: Box<dyn RowGroupReader> = row_group_reader_result.unwrap();
        assert_eq!(
            row_group_reader.num_columns(),
            row_group_metadata.num_columns()
        );
        assert_eq!(
            row_group_reader.metadata().total_byte_size(),
            row_group_metadata.total_byte_size()
        );

        // Test page readers
        // TODO: test for every column
        let page_reader_0_result = row_group_reader.get_column_page_reader(0);
        assert!(page_reader_0_result.is_ok());
        let mut page_reader_0: Box<dyn PageReader> = page_reader_0_result.unwrap();
        let mut page_count = 0;
        while let Ok(Some(page)) = page_reader_0.get_next_page() {
            let is_expected_page = match page {
                Page::DictionaryPage {
                    buf,
                    num_values,
                    encoding,
                    is_sorted,
                } => {
                    assert_eq!(buf.len(), 7);
                    assert_eq!(num_values, 1);
                    assert_eq!(encoding, Encoding::PLAIN);
                    assert!(!is_sorted);
                    true
                }
                Page::DataPageV2 {
                    buf,
                    num_values,
                    encoding,
                    num_nulls,
                    num_rows,
                    def_levels_byte_len,
                    rep_levels_byte_len,
                    is_compressed,
                    statistics,
                } => {
                    assert_eq!(buf.len(), 4);
                    assert_eq!(num_values, 5);
                    assert_eq!(encoding, Encoding::RLE_DICTIONARY);
                    assert_eq!(num_nulls, 1);
                    assert_eq!(num_rows, 5);
                    assert_eq!(def_levels_byte_len, 2);
                    assert_eq!(rep_levels_byte_len, 0);
                    assert!(is_compressed);
                    assert!(statistics.is_some());
                    true
                }
                _ => false,
            };
            assert!(is_expected_page);
            page_count += 1;
        }
        assert_eq!(page_count, 2);
    }

    #[test]
    fn test_page_iterator() {
        let file = get_test_file("alltypes_plain.parquet");
        let file_reader = Arc::new(SerializedFileReader::new(file).unwrap());

        let mut page_iterator = FilePageIterator::new(0, file_reader.clone()).unwrap();

        // read first page
        let page = page_iterator.next();
        assert!(page.is_some());
        assert!(page.unwrap().is_ok());

        // reach end of file
        let page = page_iterator.next();
        assert!(page.is_none());

        let row_group_indices = Box::new(0..1);
        let mut page_iterator =
            FilePageIterator::with_row_groups(0, row_group_indices, file_reader).unwrap();

        // read first page
        let page = page_iterator.next();
        assert!(page.is_some());
        assert!(page.unwrap().is_ok());

        // reach end of file
        let page = page_iterator.next();
        assert!(page.is_none());
    }

    #[test]
    fn test_file_reader_key_value_metadata() {
        let file = get_test_file("binary.parquet");
        let file_reader = Arc::new(SerializedFileReader::new(file).unwrap());

        let metadata = file_reader
            .metadata
            .file_metadata()
            .key_value_metadata()
            .unwrap();

        assert_eq!(metadata.len(), 3);

        assert_eq!(metadata.get(0).unwrap().key, "parquet.proto.descriptor");

        assert_eq!(metadata.get(1).unwrap().key, "writer.model.name");
        assert_eq!(metadata.get(1).unwrap().value, Some("protobuf".to_owned()));

        assert_eq!(metadata.get(2).unwrap().key, "parquet.proto.class");
        assert_eq!(
            metadata.get(2).unwrap().value,
            Some("foo.baz.Foobaz$Event".to_owned())
        );
    }

    #[test]
    fn test_file_reader_optional_metadata() {
        // file with optional metadata: bloom filters, encoding stats, column index and offset index.
        let file = get_test_file("data_index_bloom_encoding_stats.parquet");
        let file_reader = Arc::new(SerializedFileReader::new(file).unwrap());

        let row_group_metadata = file_reader.metadata.row_group(0);
        let col0_metadata = row_group_metadata.column(0);

        // test optional bloom filter offset
        assert_eq!(col0_metadata.bloom_filter_offset().unwrap(), 192);

        // test page encoding stats
        let page_encoding_stats =
            col0_metadata.page_encoding_stats().unwrap().get(0).unwrap();

        assert_eq!(page_encoding_stats.page_type, basic::PageType::DATA_PAGE);
        assert_eq!(page_encoding_stats.encoding, Encoding::PLAIN);
        assert_eq!(page_encoding_stats.count, 1);

        // test optional column index offset
        assert_eq!(col0_metadata.column_index_offset().unwrap(), 156);
        assert_eq!(col0_metadata.column_index_length().unwrap(), 25);

        // test optional offset index offset
        assert_eq!(col0_metadata.offset_index_offset().unwrap(), 181);
        assert_eq!(col0_metadata.offset_index_length().unwrap(), 11);
    }

    #[test]
    fn test_file_reader_with_no_filter() -> Result<()> {
        let test_file = get_test_file("alltypes_plain.parquet");
        let origin_reader = SerializedFileReader::new(test_file)?;
        // test initial number of row groups
        let metadata = origin_reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);
        Ok(())
    }

    #[test]
    fn test_file_reader_filter_row_groups_with_predicate() -> Result<()> {
        let test_file = get_test_file("alltypes_plain.parquet");
        let read_options = ReadOptionsBuilder::new()
            .with_predicate(Box::new(|_, _| false))
            .build();
        let reader = SerializedFileReader::new_with_options(test_file, read_options)?;
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 0);
        Ok(())
    }

    #[test]
    fn test_file_reader_filter_row_groups_with_range() -> Result<()> {
        let test_file = get_test_file("alltypes_plain.parquet");
        let origin_reader = SerializedFileReader::new(test_file)?;
        // test initial number of row groups
        let metadata = origin_reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);
        let mid = get_midpoint_offset(metadata.row_group(0));

        let test_file = get_test_file("alltypes_plain.parquet");
        let read_options = ReadOptionsBuilder::new().with_range(0, mid + 1).build();
        let reader = SerializedFileReader::new_with_options(test_file, read_options)?;
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);

        let test_file = get_test_file("alltypes_plain.parquet");
        let read_options = ReadOptionsBuilder::new().with_range(0, mid).build();
        let reader = SerializedFileReader::new_with_options(test_file, read_options)?;
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 0);
        Ok(())
    }

    #[test]
    fn test_file_reader_filter_row_groups_and_range() -> Result<()> {
        let test_file = get_test_file("alltypes_plain.parquet");
        let origin_reader = SerializedFileReader::new(test_file)?;
        let metadata = origin_reader.metadata();
        let mid = get_midpoint_offset(metadata.row_group(0));

        // true, true predicate
        let test_file = get_test_file("alltypes_plain.parquet");
        let read_options = ReadOptionsBuilder::new()
            .with_predicate(Box::new(|_, _| true))
            .with_range(mid, mid + 1)
            .build();
        let reader = SerializedFileReader::new_with_options(test_file, read_options)?;
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);

        // true, false predicate
        let test_file = get_test_file("alltypes_plain.parquet");
        let read_options = ReadOptionsBuilder::new()
            .with_predicate(Box::new(|_, _| true))
            .with_range(0, mid)
            .build();
        let reader = SerializedFileReader::new_with_options(test_file, read_options)?;
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 0);

        // false, true predicate
        let test_file = get_test_file("alltypes_plain.parquet");
        let read_options = ReadOptionsBuilder::new()
            .with_predicate(Box::new(|_, _| false))
            .with_range(mid, mid + 1)
            .build();
        let reader = SerializedFileReader::new_with_options(test_file, read_options)?;
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 0);

        // false, false predicate
        let test_file = get_test_file("alltypes_plain.parquet");
        let read_options = ReadOptionsBuilder::new()
            .with_predicate(Box::new(|_, _| false))
            .with_range(0, mid)
            .build();
        let reader = SerializedFileReader::new_with_options(test_file, read_options)?;
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 0);
        Ok(())
    }

    #[test]
    // Use java parquet-tools get below pageIndex info
    // !```
    // parquet-tools column-index ./data_index_bloom_encoding_stats.parquet
    // row group 0:
    // column index for column String:
    // Boudary order: ASCENDING
    // page-0  :
    // null count                 min                                  max
    // 0                          Hello                                today
    //
    // offset index for column String:
    // page-0   :
    // offset   compressed size       first row index
    // 4               152                     0
    ///```
    //
    fn test_page_index_reader() {
        let test_file = get_test_file("data_index_bloom_encoding_stats.parquet");
        let builder = ReadOptionsBuilder::new();
        //enable read page index
        let options = builder.with_page_index().build();
        let reader_result = SerializedFileReader::new_with_options(test_file, options);
        let reader = reader_result.unwrap();

        // Test contents in Parquet metadata
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);

        let page_indexes = metadata.page_indexes().unwrap();

        // only one row group
        assert_eq!(page_indexes.len(), 1);
        let index = if let Index::BYTE_ARRAY(index) = page_indexes.get(0).unwrap() {
            index
        } else {
            unreachable!()
        };

        assert_eq!(index.boundary_order, BoundaryOrder::Ascending);
        let index_in_pages = &index.indexes;

        //only one page group
        assert_eq!(index_in_pages.len(), 1);

        let page0 = index_in_pages.get(0).unwrap();
        let min = page0.min.as_ref().unwrap();
        let max = page0.max.as_ref().unwrap();
        assert_eq!("Hello", std::str::from_utf8(min.as_slice()).unwrap());
        assert_eq!("today", std::str::from_utf8(max.as_slice()).unwrap());

        let offset_indexes = metadata.offset_indexes().unwrap();
        // only one row group
        assert_eq!(offset_indexes.len(), 1);
        let offset_index = offset_indexes.get(0).unwrap();
        let page_offset = offset_index.get(0).unwrap();

        assert_eq!(4, page_offset.offset);
        assert_eq!(152, page_offset.compressed_page_size);
        assert_eq!(0, page_offset.first_row_index);
    }
}
