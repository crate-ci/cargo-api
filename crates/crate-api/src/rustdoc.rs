use std::collections::HashMap;
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustDocBuilder {
    deps: bool,
    target_directory: Option<std::path::PathBuf>,
}

impl RustDocBuilder {
    pub fn new() -> Self {
        Self {
            deps: false,
            target_directory: None,
        }
    }

    /// Include dependencies
    ///
    /// Reasons to have this disabled:
    /// - Faster API extraction
    /// - Less likely to hit bugs in rustdoc, like
    ///   - rust-lang/rust#89097
    ///   - rust-lang/rust#83718
    ///
    /// Reasons to have this enabled:
    /// - Check for accidental inclusion of dependencies in your API
    /// - Detect breaking changes from dependencies in your API
    pub fn deps(mut self, yes: bool) -> Self {
        self.deps = yes;
        self
    }

    pub fn target_directory(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.target_directory = Some(path.into());
        self
    }

    pub fn dump_raw(self, manifest_path: &std::path::Path) -> Result<String, crate::Error> {
        let manifest = std::fs::read_to_string(manifest_path).map_err(|e| {
            crate::Error::new(
                crate::ErrorKind::ApiParse,
                format!("Failed when reading {}: {}", manifest_path.display(), e),
            )
        })?;
        let manifest: toml_edit::Document = manifest.parse().map_err(|e| {
            crate::Error::new(
                crate::ErrorKind::ApiParse,
                format!("Failed to parse {}: {}", manifest_path.display(), e),
            )
        })?;
        let crate_name = manifest["package"]["name"].as_str().ok_or_else(|| {
            crate::Error::new(
                crate::ErrorKind::ApiParse,
                format!(
                    "Failed to parse {}: invalid package.name",
                    manifest_path.display()
                ),
            )
        })?;

        let manifest_target_directory;
        let target_dir = if let Some(target_dir) = self.target_directory.as_deref() {
            target_dir
        } else {
            let metadata = cargo_metadata::MetadataCommand::new()
                .manifest_path(manifest_path)
                .no_deps()
                .exec()
                .map_err(|e| crate::Error::new(crate::ErrorKind::ApiParse, e))?;
            manifest_target_directory = metadata
                .target_directory
                .as_path()
                .as_std_path()
                // HACK: Avoid potential errors when mixing toolchains
                .join("crate-api/target");
            manifest_target_directory.as_path()
        };

        let mut cmd = std::process::Command::new("cargo");
        cmd.env(
            "RUSTDOCFLAGS",
            "-Z unstable-options --document-hidden-items --output-format=json",
        )
        .stdout(std::process::Stdio::null()) // Don't pollute cargo api output
        .stderr(std::process::Stdio::inherit()) // Print cargo doc progress
        .args(["+nightly", "doc", "--all-features"])
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("--target-dir")
        .arg(target_dir);
        if !self.deps {
            // HACK: Trying to reduce chance of hitting
            // - rust-lang/rust#89097
            // - rust-lang/rust#83718
            cmd.arg("--no-deps");
        }

        let status = cmd
            .status()
            .map_err(|e| crate::Error::new(crate::ErrorKind::ApiParse, e))?;
        if !status.success() {
            return Err(crate::Error::new(
                crate::ErrorKind::ApiParse,
                format!(
                    "Failed when running cargo-doc on {}. See stderr.",
                    manifest_path.display(),
                ),
            ));
        }

        let json_path = target_dir.join(format!("doc/{}.json", crate_name));
        std::fs::read_to_string(&json_path).map_err(|e| {
            crate::Error::new(
                crate::ErrorKind::ApiParse,
                format!("Failed when loading {}: {}", json_path.display(), e),
            )
        })
    }

    pub fn into_api(self, manifest_path: &std::path::Path) -> Result<crate::Api, crate::Error> {
        let raw = self.dump_raw(manifest_path)?;
        parse_raw(&raw, manifest_path)
    }
}

impl Default for RustDocBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_raw(raw: &str, manifest_path: &std::path::Path) -> Result<crate::Api, crate::Error> {
    RustDocParser::new().parse(raw, manifest_path)
}

#[derive(Default)]
struct RustDocParser {
    unprocessed: VecDeque<(Option<crate::PathId>, rustdoc_json_types_fork::Id)>,
    deferred_imports: Vec<(crate::PathId, String, rustdoc_json_types_fork::Id)>,

    api: crate::Api,
    crate_ids: HashMap<u32, Option<crate::CrateId>>,
    path_ids: HashMap<rustdoc_json_types_fork::Id, Option<crate::PathId>>,
    item_ids: HashMap<rustdoc_json_types_fork::Id, Option<crate::ItemId>>,
}

impl RustDocParser {
    fn new() -> Self {
        Self::default()
    }

    fn parse(
        mut self,
        raw: &str,
        manifest_path: &std::path::Path,
    ) -> Result<crate::Api, crate::Error> {
        let raw: rustdoc_json_types_fork::Crate = serde_json::from_str(raw).map_err(|e| {
            crate::Error::new(
                crate::ErrorKind::ApiParse,
                format!(
                    "Failed when parsing json for {}: {}",
                    manifest_path.display(),
                    e
                ),
            )
        })?;

        self.unprocessed.push_back((None, raw.root.clone()));
        while let Some((parent_path_id, raw_item_id)) = self.unprocessed.pop_front() {
            let raw_item = raw
                .index
                .get(&raw_item_id)
                .expect("all item ids are in `index`");

            let crate_id = self._parse_crate(&raw, raw_item.crate_id);

            let path_id = self
                ._parse_path(&raw, parent_path_id, &raw_item_id, crate_id)
                .or(parent_path_id);

            self._parse_item(&raw, &raw_item_id, path_id, crate_id);
        }

        for (parent_path_id, name, raw_target_id) in self.deferred_imports {
            let target_path_id = self.path_ids.get(&raw_target_id).unwrap().unwrap();
            let target_path = self
                .api
                .paths
                .get(target_path_id)
                .expect("path_id to always be valid")
                .clone();

            let parent_path = self
                .api
                .paths
                .get(parent_path_id)
                .expect("all ids are valid");
            let name = format!("{}::{}", parent_path.path, name);

            let kind = crate::PathKind::Import;

            let mut path = crate::Path::new(kind, name);
            path.crate_id = parent_path.crate_id;
            path.item_id = target_path.item_id;
            path.children = target_path.children.clone();
            let path_id = self.api.paths.push(path);

            self.api
                .paths
                .get_mut(parent_path_id)
                .expect("parent_path_id to always be valid")
                .children
                .push(path_id);
        }

        Ok(self.api)
    }

    fn _parse_crate(
        &mut self,
        raw: &rustdoc_json_types_fork::Crate,
        raw_crate_id: u32,
    ) -> Option<crate::CrateId> {
        if let Some(crate_id) = self.crate_ids.get(&raw_crate_id) {
            return *crate_id;
        }

        let crate_id = (raw_crate_id != 0).then(|| {
            let raw_crate = raw
                .external_crates
                .get(&raw_crate_id)
                .expect("all crate ids are in `external_crates`");
            let crate_ = crate::Crate::new(&raw_crate.name);
            self.api.crates.push(crate_)
        });
        self.crate_ids.insert(raw_crate_id.clone(), crate_id);
        crate_id
    }

    fn _parse_path(
        &mut self,
        raw: &rustdoc_json_types_fork::Crate,
        parent_path_id: Option<crate::PathId>,
        raw_item_id: &rustdoc_json_types_fork::Id,
        crate_id: Option<crate::CrateId>,
    ) -> Option<crate::PathId> {
        if let Some(path_id) = self.path_ids.get(&raw_item_id) {
            return *path_id;
        }

        let path_id = raw.paths.get(raw_item_id).map(|raw_path| {
            let raw_item = raw
                .index
                .get(raw_item_id)
                .expect("all item ids are in `index`");

            let kind = _convert_path_kind(raw_path.kind.clone());

            let mut path = crate::Path::new(kind, raw_path.path.join("::"));
            path.crate_id = crate_id;
            path.span = raw_item.span.clone().map(|raw_span| crate::Span {
                filename: raw_span.filename,
                begin: raw_span.begin,
                end: raw_span.end,
            });
            let path_id = self.api.paths.push(path);

            if let Some(parent_path_id) = parent_path_id {
                self.api
                    .paths
                    .get_mut(parent_path_id)
                    .expect("parent_path_id to always be valid")
                    .children
                    .push(path_id);
            }
            self.api.root_id.get_or_insert(path_id);
            path_id
        });
        self.path_ids.insert(raw_item_id.clone(), path_id);
        path_id
    }

    fn _parse_item(
        &mut self,
        raw: &rustdoc_json_types_fork::Crate,
        raw_item_id: &rustdoc_json_types_fork::Id,
        path_id: Option<crate::PathId>,
        crate_id: Option<crate::CrateId>,
    ) -> Option<crate::ItemId> {
        if let Some(item_id) = self.item_ids.get(&raw_item_id) {
            return *item_id;
        }

        let raw_item = raw
            .index
            .get(raw_item_id)
            .expect("all item ids are in `index`");

        let item_id = match &raw_item.inner {
            rustdoc_json_types_fork::ItemEnum::Module(module) => {
                self.unprocessed
                    .extend(module.items.iter().map(move |i| (path_id, i.clone())));
                None
            }
            rustdoc_json_types_fork::ItemEnum::Import(import) => {
                let raw_target_id = import.id.as_ref().unwrap();
                self.unprocessed.push_back((path_id, raw_target_id.clone()));
                self.deferred_imports.push((
                    path_id.unwrap(),
                    import.name.clone(),
                    raw_target_id.clone(),
                ));
                None
            }
            rustdoc_json_types_fork::ItemEnum::Trait(trait_) => {
                self.unprocessed
                    .extend(trait_.items.iter().map(move |i| (path_id, i.clone())));
                None
            }
            rustdoc_json_types_fork::ItemEnum::Impl(impl_) => {
                self.unprocessed
                    .extend(impl_.items.iter().map(move |i| (path_id, i.clone())));
                None
            }
            rustdoc_json_types_fork::ItemEnum::Enum(enum_) => {
                self.unprocessed
                    .extend(enum_.variants.iter().map(move |i| (path_id, i.clone())));
                None
            }
            _ => {
                assert_ne!(self.api.root_id, None, "Module should be root");
                let mut item = crate::Item::new();
                item.crate_id = crate_id;
                item.name = raw_item.name.clone();
                item.span = raw_item.span.clone().map(|raw_span| crate::Span {
                    filename: raw_span.filename,
                    begin: raw_span.begin,
                    end: raw_span.end,
                });
                let item_id = self.api.items.push(item);

                if let Some(path_id) = path_id {
                    self.api
                        .paths
                        .get_mut(path_id)
                        .expect("path_id to always be valid")
                        .item_id = Some(item_id);
                }
                Some(item_id)
            }
        };
        self.item_ids.insert(raw_item_id.clone(), item_id);
        item_id
    }
}

fn _convert_path_kind(kind: rustdoc_json_types_fork::ItemKind) -> crate::PathKind {
    match kind {
        rustdoc_json_types_fork::ItemKind::Module => crate::PathKind::Module,
        rustdoc_json_types_fork::ItemKind::ExternCrate => crate::PathKind::ExternCrate,
        rustdoc_json_types_fork::ItemKind::Import => crate::PathKind::Import,
        rustdoc_json_types_fork::ItemKind::Struct => crate::PathKind::Struct,
        rustdoc_json_types_fork::ItemKind::Union => crate::PathKind::Union,
        rustdoc_json_types_fork::ItemKind::Enum => crate::PathKind::Enum,
        rustdoc_json_types_fork::ItemKind::Variant => crate::PathKind::Variant,
        rustdoc_json_types_fork::ItemKind::Function => crate::PathKind::Function,
        rustdoc_json_types_fork::ItemKind::Typedef => crate::PathKind::Typedef,
        rustdoc_json_types_fork::ItemKind::OpaqueTy => crate::PathKind::OpaqueTy,
        rustdoc_json_types_fork::ItemKind::Constant => crate::PathKind::Constant,
        rustdoc_json_types_fork::ItemKind::Trait => crate::PathKind::Trait,
        rustdoc_json_types_fork::ItemKind::TraitAlias => crate::PathKind::TraitAlias,
        rustdoc_json_types_fork::ItemKind::Method => crate::PathKind::Method,
        rustdoc_json_types_fork::ItemKind::Impl => crate::PathKind::Impl,
        rustdoc_json_types_fork::ItemKind::Static => crate::PathKind::Static,
        rustdoc_json_types_fork::ItemKind::ForeignType => crate::PathKind::ForeignType,
        rustdoc_json_types_fork::ItemKind::Macro => crate::PathKind::Macro,
        rustdoc_json_types_fork::ItemKind::ProcAttribute => crate::PathKind::ProcAttribute,
        rustdoc_json_types_fork::ItemKind::ProcDerive => crate::PathKind::ProcDerive,
        rustdoc_json_types_fork::ItemKind::AssocConst => crate::PathKind::AssocConst,
        rustdoc_json_types_fork::ItemKind::AssocType => crate::PathKind::AssocType,
        rustdoc_json_types_fork::ItemKind::Primitive => crate::PathKind::Primitive,
        rustdoc_json_types_fork::ItemKind::Keyword => crate::PathKind::Keyword,
        rustdoc_json_types_fork::ItemKind::StructField => {
            unreachable!("These are handled by the Item")
        }
    }
}
