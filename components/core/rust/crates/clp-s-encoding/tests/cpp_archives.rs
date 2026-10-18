//! Checks the encoder against the dictionaries of the C++ fixture archives: every `ClpString`
//! value that C++ `clp-s x` printed must encode to a logtype present in the archive's logtype
//! dictionary, with dictionary variables present in its variable dictionary, and must decode back
//! from those archive entries byte for byte.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clp_s_encoding::LogtypePlaceholders;
use clp_s_encoding::decode_message;
use clp_s_encoding::encode_message;

const HEADER_SIZE: usize = 64;
const METADATA_SECTION_SIZE_OFFSET: usize = 56;
const FILE_INFO_PACKET_TYPE: u8 = 1;
const TIMESTAMP_DICTIONARY_PACKET_TYPE: u8 = 2;

/// The `files` table of an archive's file-info packet.
#[derive(serde::Deserialize)]
struct FileInfo {
    files: Vec<FileEntry>,
}

/// One section of an archive: its name and its offset after the metadata section.
#[derive(serde::Deserialize)]
struct FileEntry {
    n: String,
    o: u64,
}

/// The parts of a C++ single-file archive this test reads.
struct Archive {
    sections: HashMap<String, Vec<u8>>,
    num_timestamp_entries: u64,
}

impl Archive {
    /// Factory function.
    ///
    /// Reads the metadata packets and splits the archive into its named sections.
    ///
    /// # Returns
    ///
    /// The archive on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive cannot be read or is malformed.
    fn read(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let metadata_size = usize::try_from(u32::from_le_bytes(
            bytes[METADATA_SECTION_SIZE_OFFSET..METADATA_SECTION_SIZE_OFFSET + 4].try_into()?,
        ))?;
        let sections_begin = HEADER_SIZE + metadata_size;
        let metadata = zstd::stream::decode_all(&bytes[HEADER_SIZE..sections_begin])?;

        let mut file_info = None;
        let mut num_timestamp_entries = 0;
        let mut position = 1;
        for _ in 0..metadata[0] {
            let packet_type = metadata[position];
            let size = usize::try_from(u32::from_le_bytes(
                metadata[position + 1..position + 5].try_into()?,
            ))?;
            let payload = &metadata[position + 5..position + 5 + size];
            position += 5 + size;
            match packet_type {
                FILE_INFO_PACKET_TYPE => {
                    file_info = Some(rmp_serde::from_slice::<FileInfo>(payload)?);
                }
                TIMESTAMP_DICTIONARY_PACKET_TYPE => {
                    num_timestamp_entries = u64::from_le_bytes(payload[..8].try_into()?);
                }
                _ => {}
            }
        }

        let files = file_info.context("archive has no file-info packet")?.files;
        let mut sections = HashMap::new();
        for (index, file) in files.iter().enumerate() {
            let begin = sections_begin + usize::try_from(file.o)?;
            let end = files.get(index + 1).map_or(Ok(bytes.len()), |next| {
                usize::try_from(next.o).map(|offset| sections_begin + offset)
            })?;
            sections.insert(file.n.clone(), bytes[begin..end].to_vec());
        }
        Ok(Self {
            sections,
            num_timestamp_entries,
        })
    }

    /// # Returns
    ///
    /// The entries of dictionary section `name`, in id order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the section is missing or malformed.
    fn dictionary(&self, name: &str) -> anyhow::Result<Vec<Vec<u8>>> {
        let section = self
            .sections
            .get(name)
            .with_context(|| format!("archive has no `{name}` section"))?;
        let num_entries = u64::from_le_bytes(section[..8].try_into()?);
        if 0 == num_entries {
            // C++ writes no zstd frame at all for an empty dictionary.
            return Ok(Vec::new());
        }
        let decoded = zstd::stream::decode_all(&section[8..])?;
        let mut entries = Vec::new();
        let mut position = 0;
        for _ in 0..num_entries {
            let length = usize::try_from(u64::from_le_bytes(
                decoded[position..position + 8].try_into()?,
            ))?;
            entries.push(decoded[position + 8..position + 8 + length].to_vec());
            position += 8 + length;
        }
        Ok(entries)
    }
}

/// # Returns
///
/// The corpus directory `tests/fixtures` of the prototype workspace.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// Appends every string member value containing a space (the values C++ stores as `ClpString`)
/// of `value` and its nested objects to `strings`; arrays are stored as a whole, so they are not
/// descended into.
fn collect_clp_strings(value: &serde_json::Value, strings: &mut Vec<String>) {
    let serde_json::Value::Object(members) = value else {
        return;
    };
    for member in members.values() {
        match member {
            serde_json::Value::String(text) if text.contains(' ') => strings.push(text.clone()),
            serde_json::Value::Object(_) => collect_clp_strings(member, strings),
            _ => {}
        }
    }
}

#[test]
fn clp_strings_encode_to_the_cpp_archive_dictionaries() -> anyhow::Result<()> {
    let archives_dir = fixtures_dir().join("archives");
    let mut archive_paths: Vec<_> = std::fs::read_dir(&archives_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    archive_paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| "clps" == extension)
    });
    archive_paths.sort();

    let mut num_archives_checked = 0;
    let mut num_strings_checked = 0;
    for path in &archive_paths {
        let archive = Archive::read(path)?;
        if 0 != archive.num_timestamp_entries {
            // A string at the timestamp key is stored as a timestamp, not as a `ClpString`.
            continue;
        }
        let logtypes: HashSet<Vec<u8>> = archive.dictionary("/log.dict")?.into_iter().collect();
        let variables = archive.dictionary("/var.dict")?;
        let variable_ids: HashMap<&[u8], u64> = variables
            .iter()
            .enumerate()
            .map(|(id, entry)| Ok((entry.as_slice(), u64::try_from(id)?)))
            .collect::<anyhow::Result<_>>()?;

        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("archive names are UTF-8")?;
        let oracle = std::fs::read(fixtures_dir().join("oracles").join(format!("{stem}.jsonl")))?;
        let mut strings = Vec::new();
        for line in oracle.split(|&byte| b'\n' == byte) {
            if let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) {
                collect_clp_strings(&record, &mut strings);
            }
        }

        for text in &strings {
            let mut logtype = Vec::new();
            let mut encoded_variables = Vec::new();
            let mut missing_variable = None;
            encode_message(
                text.as_bytes(),
                &mut logtype,
                &mut encoded_variables,
                |variable| {
                    variable_ids.get(variable).copied().unwrap_or_else(|| {
                        missing_variable = Some(String::from_utf8_lossy(variable).into_owned());
                        u64::MAX
                    })
                },
            );
            assert_eq!(
                missing_variable, None,
                "{stem}: dictionary variable of {text:?} is not in the variable dictionary"
            );
            assert!(
                logtypes.contains(&logtype),
                "{stem}: logtype {:?} of {text:?} is not in the logtype dictionary",
                String::from_utf8_lossy(&logtype)
            );

            let mut decoded = Vec::new();
            decode_message(
                &logtype,
                LogtypePlaceholders::new(&logtype),
                &encoded_variables,
                |id| {
                    usize::try_from(id)
                        .ok()
                        .and_then(|index| variables.get(index))
                        .map(Vec::as_slice)
                },
                &mut decoded,
            )?;
            assert_eq!(decoded, text.as_bytes(), "{stem}: decoding {text:?}");
        }
        num_archives_checked += 1;
        num_strings_checked += strings.len();
    }

    assert!(
        num_archives_checked >= 10 && num_strings_checked >= 1_000,
        "only {num_strings_checked} strings of {num_archives_checked} archives were checked"
    );
    Ok(())
}
