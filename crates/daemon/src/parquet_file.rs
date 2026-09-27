//! Data files as the publisher expects them: compressed, and never present
//! under their final name until complete.
//!
//! The publisher uploads every `{dataset}_{rfc3339}.parquet` file it finds.
//! Each file is written as `<name>.partial` and renamed once its footer is on
//! disk, so a restart mid-write leaves only a `.partial` file, which the
//! publisher ignores and pruning removes.

use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{WriterProperties, WriterPropertiesBuilder};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Suffix of a data file that is still being written.
pub const PARTIAL: &str = "partial";

/// Writer settings every data file starts from. Forecast rows carry their
/// native source provenance as JSON text, which is most of an uncompressed
/// file; zstd keeps files a small fraction of that size.
pub fn properties() -> WriterPropertiesBuilder {
    WriterProperties::builder().set_compression(Compression::ZSTD(ZstdLevel::default()))
}

/// A data file being written under its `.partial` name.
#[derive(Debug)]
pub struct PartialFile {
    partial: PathBuf,
    target: PathBuf,
}

impl PartialFile {
    /// Creates `<target>.partial` for writing.
    pub fn create(target: impl AsRef<Path>) -> io::Result<(Self, File)> {
        let target = target.as_ref().to_owned();
        let mut partial = target.clone().into_os_string();
        partial.push(format!(".{PARTIAL}"));
        let partial = PathBuf::from(partial);
        let file = File::create(&partial)?;
        Ok((Self { partial, target }, file))
    }

    /// Flushes the finished file to disk and moves it to its final name.
    pub fn commit(self) -> io::Result<()> {
        File::open(&self.partial)?.sync_all()?;
        std::fs::rename(&self.partial, &self.target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;
    use std::sync::Arc;

    fn write(target: &Path) -> PartialFile {
        let schema =
            Arc::new(parse_message_type("message rows { required binary text (UTF8); }").unwrap());
        let (output, file) = PartialFile::create(target).unwrap();
        let mut writer =
            SerializedFileWriter::new(file, schema, Arc::new(properties().build())).unwrap();
        let mut row_group = writer.next_row_group().unwrap();
        let mut column = row_group.next_column().unwrap().unwrap();
        let text = parquet::data_type::ByteArray::from("{\"layout\":\"k-p24h-n7-1\"}");
        column
            .typed::<parquet::data_type::ByteArrayType>()
            .write_batch(&vec![text; 1000], None, None)
            .unwrap();
        column.close().unwrap();
        row_group.close().unwrap();
        writer.close().unwrap();
        output
    }

    #[test]
    fn files_appear_under_their_final_name_only_once_complete() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory
            .path()
            .join("forecasts_2030-01-01T00:00:00Z.parquet");
        let output = write(&target);
        assert!(!target.exists(), "an unfinished file is not publishable");
        output.commit().unwrap();
        assert!(target.exists());
        let names: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![target.file_name().unwrap().to_owned()]);
    }

    #[test]
    fn data_files_are_zstd_compressed() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory
            .path()
            .join("forecasts_2030-01-01T00:00:00Z.parquet");
        write(&target).commit().unwrap();
        let reader = SerializedFileReader::new(File::open(&target).unwrap()).unwrap();
        let metadata = reader.metadata();
        assert!(metadata.num_row_groups() > 0);
        for row_group in metadata.row_groups() {
            for column in row_group.columns() {
                assert!(matches!(column.compression(), Compression::ZSTD(_)));
            }
        }
    }
}
