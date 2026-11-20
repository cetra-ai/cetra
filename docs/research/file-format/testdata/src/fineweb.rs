//! FineWeb text cut into fixed-size pieces, for the Braintrust comparison
//! (`real-data.md` § Braintrust's search benchmark). Braintrust ran its search
//! benchmark on 4 million spans of 10 KB of FineWeb text; each piece here is
//! one span's input.
//!
//! Input: the Parquet files of `HuggingFaceFW/fineweb` (`sample/10BT`), read
//! for their `text` column in order. Documents are joined with a blank line
//! and cut into pieces of `size` bytes, ended at a character boundary, so no
//! text is repeated or dropped between pieces.

use std::path::PathBuf;

use arrow::array::{Array, AsArray, RecordBatchReader};
use arrow::datatypes::DataType;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Pieces of FineWeb text of about `size` bytes, read from `paths` in order.
pub struct Pieces {
    paths: Vec<PathBuf>,
    next_path: usize,
    reader: Option<Box<dyn RecordBatchReader + Send>>,
    /// Documents of the current batch not used yet, last first.
    docs: Vec<String>,
    /// Text read but not cut into a piece yet.
    buffer: String,
    size: usize,
}

impl Pieces {
    pub fn new(paths: Vec<PathBuf>, size: usize) -> Self {
        Self { paths, next_path: 0, reader: None, docs: Vec::new(), buffer: String::new(), size }
    }

    /// The next document, or `None` when every file is read.
    fn next_doc(&mut self) -> R<Option<String>> {
        loop {
            if let Some(doc) = self.docs.pop() {
                return Ok(Some(doc));
            }
            if let Some(reader) = self.reader.as_mut() {
                match reader.next() {
                    Some(batch) => {
                        let batch = batch?;
                        let column = batch.column(0);
                        self.docs = match column.data_type() {
                            DataType::LargeUtf8 => {
                                let text = column.as_string::<i64>();
                                (0..text.len()).rev().filter(|&i| text.is_valid(i)).map(|i| text.value(i).to_owned()).collect()
                            }
                            DataType::Utf8 => {
                                let text = column.as_string::<i32>();
                                (0..text.len()).rev().filter(|&i| text.is_valid(i)).map(|i| text.value(i).to_owned()).collect()
                            }
                            other => return Err(format!("fineweb parquet: text is {other}").into()),
                        };
                        continue;
                    }
                    None => self.reader = None,
                }
            }
            let Some(path) = self.paths.get(self.next_path) else {
                return Ok(None);
            };
            self.next_path += 1;
            let builder = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path)?)?;
            let text = builder.parquet_schema().columns().iter().position(|c| c.name() == "text").ok_or("fineweb parquet: no text column")?;
            let mask = ProjectionMask::leaves(builder.parquet_schema(), [text]);
            self.reader = Some(Box::new(builder.with_projection(mask).with_batch_size(1024).build()?));
        }
    }

    /// The next piece, or `None` when the files hold less than a whole piece more.
    pub fn next_piece(&mut self) -> R<Option<String>> {
        while self.buffer.len() < self.size {
            match self.next_doc()? {
                Some(doc) => {
                    if !self.buffer.is_empty() {
                        self.buffer.push_str("\n\n");
                    }
                    self.buffer.push_str(&doc);
                }
                None => return Ok(None),
            }
        }
        let cut = self.buffer.floor_char_boundary(self.size);
        let rest = self.buffer.split_off(cut);
        Ok(Some(std::mem::replace(&mut self.buffer, rest)))
    }
}
