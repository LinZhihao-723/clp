//! Archive-scoped render caches.

use std::collections::HashMap;
use std::sync::Arc;

use clp_s_format::SchemaId;
use clp_s_format::TableOrdinal;
use clp_s_table::ArchiveTables;
use clp_s_table::ResolvedProjection;
use clp_s_table::TableError;
use parking_lot::Mutex;

use crate::JsonError;
use crate::JsonOptions;
use crate::JsonTemplate;
use crate::logtype::LogtypeCache;

/// Archive-scoped render caches: compiled logtypes and templates (whose fragments hold the
/// pre-escaped keys), all built lazily on first use.
#[derive(Debug)]
pub struct RenderContext {
    tables: Arc<ArchiveTables>,
    logtypes: LogtypeCache,

    /// The compiled templates of each (schema, options), one per distinct projection.
    templates: Mutex<HashMap<TemplateKey, Vec<CachedTemplate>>>,
}

impl RenderContext {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty context for the archive of `tables`.
    #[must_use]
    pub fn new(tables: Arc<ArchiveTables>) -> Self {
        Self {
            tables,
            logtypes: LogtypeCache::default(),
            templates: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the template for (table, projection, options), compiling it on first use.
    ///
    /// # Returns
    ///
    /// The template on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ArchiveTables::layout`]'s return values on failure.
    /// * Forwards [`clp_s_table::TableLayout::projected_shape`]'s return values on failure.
    /// * Forwards [`JsonTemplate::compile`]'s return values on failure.
    pub fn template(
        &self,
        table: TableOrdinal,
        projection: &ResolvedProjection,
        options: &JsonOptions,
    ) -> Result<Arc<JsonTemplate>, JsonError> {
        let layout = self.tables.layout(table).map_err(TableError::from)?;
        let key = TemplateKey {
            schema_id: layout.schema_id(),
            options: *options,
        };
        let cached = Self::find(&self.templates.lock(), key, projection);
        if let Some(template) = cached {
            return Ok(template);
        }

        let index = self.tables.index();
        let tree = index.schema_tree();
        let template = Arc::new(match projection {
            ResolvedProjection::Full => {
                JsonTemplate::compile(tree, &layout, layout.shape().events(), *options)?
            }
            ResolvedProjection::Nodes(_) => {
                let shape = layout
                    .projected_shape(index, projection)
                    .map_err(TableError::from)?;
                JsonTemplate::compile(tree, &layout, shape.events(), *options)?
            }
        });

        let mut templates = self.templates.lock();
        let entries = templates.entry(key).or_default();
        if let Some(existing) = entries
            .iter()
            .find(|cached| cached.projection == *projection)
        {
            return Ok(Arc::clone(&existing.template));
        }
        entries.push(CachedTemplate {
            projection: projection.clone(),
            template: Arc::clone(&template),
        });
        drop(templates);
        Ok(template)
    }

    /// # Returns
    ///
    /// The table layer of the archive.
    #[must_use]
    pub const fn tables(&self) -> &Arc<ArchiveTables> {
        &self.tables
    }

    /// # Returns
    ///
    /// The compiled logtypes of the archive's logtype dictionary.
    pub(crate) const fn logtypes(&self) -> &LogtypeCache {
        &self.logtypes
    }

    /// # Returns
    ///
    /// The cached template of (`key`, `projection`), if compiled.
    fn find(
        templates: &HashMap<TemplateKey, Vec<CachedTemplate>>,
        key: TemplateKey,
        projection: &ResolvedProjection,
    ) -> Option<Arc<JsonTemplate>> {
        templates
            .get(&key)?
            .iter()
            .find(|cached| cached.projection == *projection)
            .map(|cached| Arc::clone(&cached.template))
    }
}

/// A compiled template and the projection it renders.
#[derive(Debug)]
struct CachedTemplate {
    projection: ResolvedProjection,
    template: Arc<JsonTemplate>,
}

/// The part of a template's identity that can be hashed; projections are compared by equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TemplateKey {
    schema_id: SchemaId,
    options: JsonOptions,
}
