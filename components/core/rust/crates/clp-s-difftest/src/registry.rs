//! The machine-checked divergence registry (design report §8.3, restricted to this round's scope by
//! `docs/DESIGN.md` §10), loaded from `tests/divergences.toml`.

use std::collections::BTreeMap;
use std::path::Path;

/// The registry file, relative to the prototype root.
pub const REGISTRY_PATH: &str = "tests/divergences.toml";

/// The id of a registered divergence (`DV-01`..`DV-47`) or scope exclusion (`SX-01`..).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct DivergenceId(String);

impl DivergenceId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if `text` is not `DV-` or `SX-` followed by two digits.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let is_valid = text.len() == 5
            && (text.starts_with("DV-") || text.starts_with("SX-"))
            && text[3..].bytes().all(|byte| byte.is_ascii_digit());
        if !is_valid {
            anyhow::bail!("invalid divergence id {text:?}");
        }
        Ok(Self(text.to_owned()))
    }

    /// # Returns
    ///
    /// The id text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for DivergenceId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DivergenceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How the harness recognizes a divergence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Classifier {
    /// Records compare equal after the DV-01/DV-02 float normalization.
    FloatNormalization,

    /// The case must declare the divergence; Rust must agree with the reference evaluator over the
    /// input JSON, and C++ must differ.
    DeclaredByCase,

    /// The archive-dependence rule of design report §8.3: C++ on the single record agrees with Rust
    /// while C++ on the full archive does not. Declared cases are checked like
    /// [`Self::DeclaredByCase`].
    ArchiveDependence,

    /// Checked by a dedicated output-shape test (wire, results cache, exit codes), not by record
    /// comparison.
    OutputShape,

    /// Rust rejects the input with the declared error kind before producing any result.
    ExpectedRejection,

    /// Proven by a unit test of the owning crate; never accepts a record mismatch.
    DedicatedTest,
}

/// The predicate whose result flips with the archive's other records, which suggests an
/// archive-dependence entry for an undeclared mismatch (design report §8.3 classifier).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveDependenceRule {
    /// `NOT p: *`.
    NotExistsPath,

    /// A pure wildcard or a bare value under `NOT`.
    NegatedWildcardOrBareValue,

    /// `*: null`.
    WildcardNull,

    /// A partial wildcard over a record with arrays.
    PartialWildcardOverArray,

    /// Order-dependent with arrays.
    OrderDependentArray,

    /// Order-dependent with a `""` key in the record.
    OrderDependentEmptyKey,

    /// A wildcard with a `""` key in the record and no arrays.
    WildcardEmptyKey,
}

/// One registry entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    pub id: DivergenceId,
    pub summary: String,
    pub classifier: Classifier,

    /// How a mismatch caused by this entry looks.
    pub recognition: String,

    /// Where the positive test lives.
    pub checked_by: String,

    pub archive_dependence_rule: Option<ArchiveDependenceRule>,
}

/// The registry: every divergence and scope exclusion in force for this round.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    entries: BTreeMap<DivergenceId, Divergence>,
}

impl Registry {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The registry read from `root`'s [`REGISTRY_PATH`] on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the file cannot be read.
    /// * Forwards [`Self::from_toml`]'s return values on failure.
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let path = root.join(REGISTRY_PATH);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
        Self::from_toml(&text)
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The registry parsed from TOML text on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if:
    ///   * The text is not a valid registry document.
    ///   * An id is invalid or repeated.
    ///   * An `archive_dependence_rule` is given to an entry whose classifier is not
    ///     [`Classifier::ArchiveDependence`], or missing from one whose classifier is.
    /// * Forwards [`DivergenceId::parse`]'s return values on failure.
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let document: RegistryDocument = toml::from_str(text)?;
        let mut entries = BTreeMap::new();
        for raw in document.divergence {
            let id = DivergenceId::parse(&raw.id)?;
            let is_archive_dependence = raw.classifier == Classifier::ArchiveDependence;
            if is_archive_dependence != raw.archive_dependence_rule.is_some() {
                anyhow::bail!(
                    "{id}: `archive_dependence_rule` must be given exactly for archive-dependence \
                     entries"
                );
            }
            let divergence = Divergence {
                id: id.clone(),
                summary: raw.summary,
                classifier: raw.classifier,
                recognition: raw.recognition,
                checked_by: raw.checked_by,
                archive_dependence_rule: raw.archive_dependence_rule,
            };
            if entries.insert(id.clone(), divergence).is_some() {
                anyhow::bail!("{id}: repeated divergence id");
            }
        }
        Ok(Self { entries })
    }

    /// # Returns
    ///
    /// The entry with the id `id`, if registered.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Divergence> {
        self.entries.get(id)
    }

    /// # Returns
    ///
    /// Every entry, sorted by id.
    pub fn entries(&self) -> impl Iterator<Item = &Divergence> {
        self.entries.values()
    }

    /// Resolves the divergences a query case declares, each of which must be registered with a
    /// classifier that a case may declare.
    ///
    /// # Returns
    ///
    /// The entries in declaration order on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if an id is not registered, or its classifier is one a case cannot
    ///   declare ([`Classifier::FloatNormalization`], [`Classifier::ExpectedRejection`] or
    ///   [`Classifier::DedicatedTest`]).
    pub fn resolve_declared(&self, ids: &[String]) -> anyhow::Result<Vec<&Divergence>> {
        ids.iter()
            .map(|id| {
                let entry = self
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("divergence {id} is not registered"))?;
                match entry.classifier {
                    Classifier::DeclaredByCase
                    | Classifier::ArchiveDependence
                    | Classifier::OutputShape => Ok(entry),
                    Classifier::FloatNormalization
                    | Classifier::ExpectedRejection
                    | Classifier::DedicatedTest => anyhow::bail!(
                        "divergence {id} is classified {:?}, which a query case cannot declare",
                        entry.classifier
                    ),
                }
            })
            .collect()
    }

    /// # Returns
    ///
    /// The archive-dependence entries whose rule is `rule`.
    pub fn with_rule(&self, rule: ArchiveDependenceRule) -> impl Iterator<Item = &Divergence> {
        self.entries
            .values()
            .filter(move |entry| entry.archive_dependence_rule == Some(rule))
    }
}

/// The TOML document of [`REGISTRY_PATH`].
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryDocument {
    divergence: Vec<RawDivergence>,
}

/// One `[[divergence]]` table.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDivergence {
    id: String,
    summary: String,
    classifier: Classifier,
    recognition: String,
    checked_by: String,
    archive_dependence_rule: Option<ArchiveDependenceRule>,
}

#[cfg(test)]
mod tests {
    use super::ArchiveDependenceRule;
    use super::Classifier;
    use super::DivergenceId;
    use super::Registry;
    use crate::corpus::load_queries;
    use crate::corpus::prototype_root;

    const ENTRY: &str = r#"
[[divergence]]
id = "DV-07"
summary = "s"
classifier = "archive-dependence"
recognition = "r"
checked_by = "c"
archive_dependence_rule = "not-exists-path"

[[divergence]]
id = "DV-01"
summary = "s"
classifier = "float-normalization"
recognition = "r"
checked_by = "c"
"#;

    #[test]
    fn the_registry_file_loads_and_covers_the_harness_ids() -> anyhow::Result<()> {
        let registry = Registry::load(&prototype_root())?;
        for (id, classifier) in [
            ("DV-01", Classifier::FloatNormalization),
            ("DV-02", Classifier::FloatNormalization),
            ("DV-21", Classifier::OutputShape),
            ("DV-24", Classifier::OutputShape),
            ("DV-25", Classifier::OutputShape),
            ("SX-01", Classifier::ExpectedRejection),
            ("SX-02", Classifier::ExpectedRejection),
        ] {
            assert_eq!(
                registry.get(id).map(|entry| entry.classifier),
                Some(classifier),
                "{id}"
            );
        }
        let ids: Vec<&str> = registry.entries().map(|entry| entry.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        for out_of_scope in [
            "DV-03", "DV-04", "DV-26", "DV-33", "DV-40", "DV-44", "DV-46",
        ] {
            assert_eq!(
                registry.get(out_of_scope),
                None,
                "{out_of_scope} is out of scope"
            );
        }
        Ok(())
    }

    #[test]
    fn every_divergence_the_suite_declares_is_registered_and_declarable() -> anyhow::Result<()> {
        let root = prototype_root();
        let registry = Registry::load(&root)?;
        for case in load_queries(&root)? {
            let declared = registry.resolve_declared(&case.divergences)?;
            assert_eq!(declared.len(), case.divergences.len(), "{}", case.id);
        }
        Ok(())
    }

    #[test]
    fn every_suite_case_a_divergence_names_exists_and_declares_it() -> anyhow::Result<()> {
        let root = prototype_root();
        let registry = Registry::load(&root)?;
        let cases = load_queries(&root)?;
        let mut missing = Vec::new();
        for entry in registry.entries() {
            let Some((_, suite_part)) = entry.checked_by.split_once("suite:") else {
                assert!(
                    !matches!(
                        entry.classifier,
                        Classifier::DeclaredByCase | Classifier::ArchiveDependence
                    ),
                    "{} accepts record mismatches but names no suite case",
                    entry.id
                );
                continue;
            };
            let names = suite_part.split(';').next().unwrap_or_default();
            for name in names.split(',') {
                let name = name.trim().trim_end_matches(" and others");
                let name = name.split_once(' ').map_or(name, |(head, _)| head);
                let declares = cases.iter().any(|case| {
                    case.id == name && case.divergences.iter().any(|id| id == entry.id.as_str())
                });
                if !declares {
                    missing.push(format!("{}: {name}", entry.id));
                }
            }
        }
        assert_eq!(missing, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn every_archive_dependence_rule_has_an_entry() -> anyhow::Result<()> {
        let registry = Registry::load(&prototype_root())?;
        for rule in [
            ArchiveDependenceRule::NotExistsPath,
            ArchiveDependenceRule::NegatedWildcardOrBareValue,
            ArchiveDependenceRule::WildcardNull,
            ArchiveDependenceRule::PartialWildcardOverArray,
            ArchiveDependenceRule::OrderDependentArray,
            ArchiveDependenceRule::OrderDependentEmptyKey,
            ArchiveDependenceRule::WildcardEmptyKey,
        ] {
            assert_eq!(registry.with_rule(rule).count(), 1, "{rule:?}");
        }
        Ok(())
    }

    #[test]
    fn from_toml_parses_and_sorts_entries() -> anyhow::Result<()> {
        let registry = Registry::from_toml(ENTRY)?;
        let ids: Vec<&str> = registry.entries().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, ["DV-01", "DV-07"]);
        assert_eq!(
            registry
                .with_rule(ArchiveDependenceRule::NotExistsPath)
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["DV-07"]
        );
        Ok(())
    }

    #[test]
    fn from_toml_rejects_inconsistent_registries() {
        let cases = [
            ("repeated id", format!("{ENTRY}{ENTRY}")),
            ("invalid id", ENTRY.replace("DV-07", "DV-7")),
            (
                "rule on a non-archive-dependence entry",
                ENTRY.replace(
                    "classifier = \"float-normalization\"",
                    "classifier = \"float-normalization\"\narchive_dependence_rule = \
                     \"wildcard-null\"",
                ),
            ),
            (
                "archive-dependence entry without a rule",
                ENTRY.replace("archive_dependence_rule = \"not-exists-path\"", ""),
            ),
            (
                "unknown classifier",
                ENTRY.replace("float-normalization", "guess"),
            ),
            ("unknown field", format!("{ENTRY}extra = 1\n")),
        ];
        for (description, text) in cases {
            assert!(
                Registry::from_toml(&text).is_err(),
                "{description} must be rejected"
            );
        }
    }

    #[test]
    fn resolve_declared_rejects_unknown_and_undeclarable_ids() -> anyhow::Result<()> {
        let registry = Registry::from_toml(ENTRY)?;
        assert_eq!(registry.resolve_declared(&["DV-07".to_owned()])?.len(), 1);
        assert!(registry.resolve_declared(&["DV-99".to_owned()]).is_err());
        assert!(registry.resolve_declared(&["DV-01".to_owned()]).is_err());
        Ok(())
    }

    #[test]
    fn divergence_ids_are_validated() -> anyhow::Result<()> {
        assert_eq!(DivergenceId::parse("SX-03")?.to_string(), "SX-03");
        for invalid in ["DV-1", "DV-001", "XX-01", "DV-0a", ""] {
            assert!(DivergenceId::parse(invalid).is_err(), "{invalid:?}");
        }
        Ok(())
    }
}
