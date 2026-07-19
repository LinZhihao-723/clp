//! Round-robin iteration over S3 objects grouped by filename similarity.

use std::{collections::VecDeque, path::Path};

use clp_rust_utils::s3::ObjectMetadata;

/// Owns filename-similarity groups and returns one object from each group in round-robin order.
pub(crate) struct RoundRobinPartition {
    /// Non-empty groups containing objects that have not yet been returned.
    groups: VecDeque<std::vec::IntoIter<ObjectMetadata>>,
}

impl RoundRobinPartition {
    /// Groups objects by filename similarity and creates a round-robin iterator over the groups.
    ///
    /// # Parameters
    ///
    /// * `files` - The S3 objects to group and iterate over.
    ///
    /// # Returns
    ///
    /// A new [`RoundRobinPartition`].
    pub(crate) fn new(files: Vec<ObjectMetadata>) -> Self {
        let groups = group_files_by_similar_filenames(files)
            .into_iter()
            .filter(|group| !group.is_empty())
            .map(|group| group.into_iter())
            .collect();

        Self { groups }
    }

    /// Consumes the iterator and collects every object that has not yet been returned.
    ///
    /// # Returns
    ///
    /// The remaining objects, flattened according to the iterator's current group order.
    pub(crate) fn into_remaining_files(self) -> Vec<ObjectMetadata> {
        self.groups.into_iter().flatten().collect()
    }
}

impl Iterator for RoundRobinPartition {
    /// An S3 object selected from the next available filename-similarity group.
    type Item = ObjectMetadata;

    /// Removes one object from the front group and moves that group to the back if it still
    /// contains objects.
    ///
    /// # Returns
    ///
    /// The next S3 object, or `None` when every group is exhausted.
    fn next(&mut self) -> Option<Self::Item> {
        let mut group = self.groups.pop_front()?;
        let file = group.next()?;
        if !group.as_slice().is_empty() {
            self.groups.push_back(group);
        }
        Some(file)
    }
}

/// Gets the filename portion of an S3 object's key.
///
/// # Parameters
///
/// * `metadata` - Metadata containing the S3 object key.
///
/// # Returns
///
/// The key's filename when it is valid UTF-8, or the complete key otherwise.
fn filename(metadata: &ObjectMetadata) -> &str {
    Path::new(metadata.key.as_str())
        .file_name()
        .and_then(|filename| filename.to_str())
        .unwrap_or_else(|| metadata.key.as_str())
}

/// Sorts S3 objects by filename and divides adjacent objects into similarity groups.
///
/// A new group begins whenever the normalized Levenshtein similarity between adjacent filenames
/// falls below the configured threshold.
///
/// # Parameters
///
/// * `files` - The S3 objects to group.
///
/// # Returns
///
/// The filename-similarity groups, or an empty vector when `files` is empty.
fn group_files_by_similar_filenames(mut files: Vec<ObjectMetadata>) -> Vec<Vec<ObjectMetadata>> {
    /// The minimum normalized Levenshtein similarity for adjacent filenames to share a group.
    const FILE_GROUPING_MIN_LEVENSHTEIN_RATIO: f64 = 0.6;

    files.sort_by(|a, b| filename(a).cmp(filename(b)));

    let mut files = files.into_iter();
    let Some(first_file) = files.next() else {
        return Vec::new();
    };

    let mut previous_filename = filename(&first_file).to_owned();
    let mut current_group = vec![first_file];
    let mut groups = Vec::new();

    for file in files {
        let current_filename = filename(&file).to_owned();
        if strsim::normalized_levenshtein(&previous_filename, &current_filename)
            < FILE_GROUPING_MIN_LEVENSHTEIN_RATIO
        {
            groups.push(current_group);
            current_group = Vec::new();
        }
        current_group.push(file);
        previous_filename = current_filename;
    }
    groups.push(current_group);

    groups
}
