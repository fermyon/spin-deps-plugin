use anyhow::{anyhow, bail, Result};
use clap::Args;
use http::HttpAddCommand;
use local::LocalAddCommand;
use registry::RegistryAddCommand;
use semver::VersionReq;
use spin_manifest::{
    manifest_from_file,
    schema::v2::{AppManifest, ComponentDependency},
};
use spin_serde::{DependencyName, DependencyPackageName, KebabId};
use std::{collections::HashMap, path::{Path, PathBuf}};
use tokio::fs;
use url::Url;
use wasm_pkg_client::{PackageRef, Registry};
use wit_parser::{PackageId, Resolve};

use crate::common::{
    constants::{SPIN_DEPS_WIT_FILE_NAME, SPIN_WIT_DIRECTORY},
    interact::{select_multiple_prompt, select_prompt},
    manifest::{edit_component_deps_in_manifest, get_component_ids, get_spin_manifest_path},
    wit::{
        get_exported_interfaces, merge_dependecy_package, parse_component_bytes, resolve_to_wit,
    },
};

mod http;
mod local;
mod registry;

#[derive(Args, Debug)]
pub struct AddCommand {
    /// Source to the component. Can be one of a local path, a HTTP URL or a registry reference.
    pub source: String,
    /// Sha256 digest that will be used to verify HTTP downloads. Required for HTTP sources, ignored otherwise.
    #[clap(short, long)]
    pub digest: Option<String>,
    /// Registry to override the default with. Ignored in the cases of local or HTTP sources.
    #[clap(short, long)]
    pub registry: Option<Registry>,
    /// The Spin component to add the dependency to. If omitted, it is prompted for.
    #[clap(long = "to")]
    pub add_to_component: Option<String>,
    /// The path to the manifest. This can be a file or directory. The default is 'spin.toml'.
    #[clap(short = 'f')]
    pub manifest_path: Option<PathBuf>,
}

enum ComponentSource {
    Local(LocalAddCommand),
    Http(HttpAddCommand),
    Registry(RegistryAddCommand),
}

impl ComponentSource {
    pub fn infer_source(
        source: &String,
        digest: &Option<String>,
        registry: &Option<Registry>,
    ) -> Result<Self> {
        let path = PathBuf::from(&source);
        if path.exists() {
            return Ok(Self::Local(LocalAddCommand { path }));
        }

        if let Ok(url) = Url::parse(source) {
            if url.scheme().starts_with("http") {
                return digest.clone().map_or_else(
                    || bail!("Digest needs to be specified for HTTP sources."),
                    |d| Ok(Self::Http(HttpAddCommand { url, digest: d })),
                );
            }
        }

        if let Ok((name, version)) = package_name_ver(source) {
            if version.is_none() {
                bail!("Version needs to specified for registry sources.")
            }
            return Ok(Self::Registry(RegistryAddCommand {
                package: name,
                version: version.unwrap(),
                registry: registry.clone(),
            }));
        }

        bail!("Could not infer component source");
    }

    pub async fn get_component(&self) -> Result<Vec<u8>> {
        match &self {
            ComponentSource::Local(cmd) => cmd.get_component().await,
            ComponentSource::Http(cmd) => cmd.get_component().await,
            ComponentSource::Registry(cmd) => cmd.get_component().await,
        }
    }
}

impl AddCommand {
    pub async fn run(&self) -> Result<()> {
        let source = ComponentSource::infer_source(&self.source, &self.digest, &self.registry)?;

        let component = source.get_component().await?;

        let (mut resolve, main) = parse_component_bytes(component)?;

        let selected_interfaces = self.select_interfaces(&mut resolve, main)?;

        let (manifest_file, distance) = spin_common::paths::find_manifest_file_path(self.manifest_path.as_ref())?;
        if distance > 0 {
            anyhow::bail!(
                "No spin.toml in current directory - did you mean '-f {}'?",
                manifest_file.display()
            );
        }

        let mut manifest = manifest_from_file(&manifest_file)?;

        let selected_component = self.target_component(&manifest)?;

        resolve.importize(
            resolve.select_world(main, None)?,
            Some("dependency-world".to_string()),
        )?;

        let component_dir = PathBuf::from(SPIN_WIT_DIRECTORY).join(&selected_component);

        let output_wit = component_dir.join(SPIN_DEPS_WIT_FILE_NAME);

        let base_resolve_file = if std::fs::exists(&output_wit)? {
            Some(&output_wit)
        } else {
            fs::create_dir_all(&component_dir).await?;
            None
        };

        let (merged_resolve, main) = merge_dependecy_package(base_resolve_file, &resolve, main)?;
        let wit_text = resolve_to_wit(&merged_resolve, main)?;
        fs::write(output_wit, wit_text).await?;

        self.update_manifest(
            source,
            &mut manifest,
            &selected_component,
            &selected_interfaces,
        )
        .await?;

        let target_component_id = KebabId::try_from(selected_component.clone()).map_err(|e| anyhow!("{e}"))?;
        let target_component = manifest.components.get(&target_component_id).ok_or_else(|| anyhow!("component does not exist"))?;
        let target = BindOMatic {
            // manifest: &manifest,
            root_dir: manifest_file.parent().ok_or_else(|| anyhow!("Manifest cannot be the root directory"))?,
            target_component,
            component_id: &selected_component,
            interfaces: &selected_interfaces
        };
        try_generate_bindings(&target)?;

        Ok(())
    }

    fn target_component(&self, manifest: &AppManifest) -> anyhow::Result<String> {
        if let Some(id) = &self.add_to_component {
            return Ok(id.to_owned())
        }

        let component_ids = get_component_ids(&manifest);
        let selected_component_index = select_prompt(
            "Select a component to add the dependency to",
            &component_ids,
            None,
        )?;
        let selected_component = &component_ids[selected_component_index];

        Ok(selected_component.clone())
    }

    /// Prompts the user to select an interface to import.
    fn select_interfaces(&self, resolve: &mut Resolve, main: PackageId) -> Result<Vec<String>> {
        let world_id = resolve.select_world(main, None)?;
        let exported_interfaces = get_exported_interfaces(resolve, world_id);

        if exported_interfaces.is_empty() {
            bail!("No exported interfaces found in the component")
        };

        let mut package_interface_map: HashMap<_, Vec<String>> = HashMap::new();

        // Map interfaces to their respective packages
        for (package_name, interface) in exported_interfaces {
            package_interface_map
                .entry(package_name)
                .or_default()
                .push(interface);
        }

        let package_names: Vec<_> = package_interface_map.keys().cloned().collect();

        let selected_package_indices = select_multiple_prompt(
            "Select packages to import (use space to select, enter to confirm)",
            &package_names,
        )?;

        let mut selected_interfaces = Vec::new();

        for &package_idx in selected_package_indices.iter() {
            let package_name = &package_names[package_idx];
            let interfaces = package_interface_map.get(package_name).unwrap();
            let interface_count = interfaces.len();

            // If there's only one interface, skip the "Import all" option
            let interface_options: Vec<String> = if interface_count > 1 {
                std::iter::once("(Import all interfaces)".to_string())
                    .chain(interfaces.clone())
                    .collect()
            } else {
                interfaces.clone()
            };

            // Prompt user to select an interface
            let selected_interface_idx = select_prompt(
                &format!(
                    "Select one or all interfaces to import from package '{}'",
                    package_name
                ),
                &interface_options,
                Some(0),
            )?;

            if interface_count > 1 && selected_interface_idx == 0 {
                selected_interfaces.push(package_name.to_string());
            } else {
                let interface_name = &interface_options[selected_interface_idx];
                let full_itf_name = if let Some(version) = package_name.version.as_ref() {
                    format!(
                        "{ns}:{name}/{interface_name}@{version}",
                        ns = package_name.namespace,
                        name = package_name.name
                    )
                } else {
                    format!("{package_name}/{interface_name}")
                };
                selected_interfaces.push(full_itf_name);
            }
        }

        Ok(selected_interfaces)
    }

    /// Updates the manifest file with the new component dependency.
    async fn update_manifest(
        &self,
        source: ComponentSource,
        manifest: &mut AppManifest,
        selected_component: &str,
        selected_interfaces: &[String],
    ) -> Result<()> {
        let id = KebabId::try_from(selected_component.to_owned()).unwrap();
        let component = manifest.components.get_mut(&id).unwrap();

        let component_dependency = match source {
            ComponentSource::Local(src) => ComponentDependency::Local {
                path: src.path.clone(),
                export: None,
            },
            ComponentSource::Http(src) => ComponentDependency::HTTP {
                url: src.url.to_string(),
                digest: format!("sha256:{}", src.digest.clone()),
                export: None,
            },
            ComponentSource::Registry(src) => ComponentDependency::Package {
                version: src.version.to_string(),
                registry: src.registry.as_ref().map(|registry| registry.to_string()),
                package: Some(src.package.clone().to_string()),
                export: None,
            },
        };

        for interface in selected_interfaces {
            component.dependencies.inner.insert(
                DependencyName::Package(DependencyPackageName::try_from(interface.clone())?),
                component_dependency.clone(),
            );
        }

        let doc =
            edit_component_deps_in_manifest(selected_component, &component.dependencies).await?;

        let manifest_path = get_spin_manifest_path()?;
        fs::write(manifest_path, doc).await?;

        Ok(())
    }
}

fn package_name_ver(package_name: &str) -> Result<(PackageRef, Option<VersionReq>)> {
    let (package, version) = package_name
        .split_once('@')
        .map(|(pkg, ver)| (pkg, Some(ver)))
        .unwrap_or((package_name, None));

    let version = if let Some(v) = version {
        Some(v.parse()?)
    } else {
        None
    };
    Ok((package.parse()?, version))
}

struct BindOMatic<'a> {
    // manifest: &'a AppManifest,
    root_dir: &'a Path,
    target_component: &'a spin_manifest::schema::v2::Component,
    component_id: &'a str,
    interfaces: &'a [String],
}

enum Language {
    Rust { cargo_toml: PathBuf },
    TypeScript { package_json: PathBuf },
}

impl<'a> BindOMatic<'a> {
    fn try_infer_language(&self) -> anyhow::Result<Language> {
        let workdir = self.target_component.build.as_ref().and_then(|b| b.workdir.as_ref());
        let build_dir = match workdir {
            None => self.root_dir.to_owned(),
            Some(d) => self.root_dir.join(d),
        };

        if !build_dir.is_dir() {
            bail!("unable to establish build directory for component");
        }

        let cargo_toml = build_dir.join("Cargo.toml");
        if cargo_toml.is_file() {
            return Ok(Language::Rust { cargo_toml });
        }
        let package_json = build_dir.join("package.json");
        if package_json.is_file() {
            // TODO: yes also JavaScript
            return Ok(Language::TypeScript { package_json });
        }

        Err(anyhow!("unable to determine the component source language"))
    }
}

fn try_generate_bindings(target: &BindOMatic) -> anyhow::Result<()> {
    match target.try_infer_language()? {
        Language::Rust { cargo_toml } => generate_rust_bindings(target.root_dir, &cargo_toml, target.component_id, target.interfaces),
        Language::TypeScript { package_json } => todo!(),
    }
}

fn generate_rust_bindings(root_dir: &Path, cargo_toml: &Path, component_id: &str, interfaces: &[String]) -> anyhow::Result<()> {
    // add wit-bindgen to cargo.toml if needed
    let mut did_change = false;
    let cargo_text = std::fs::read_to_string(cargo_toml)?;
    let mut cargo_doc: toml_edit::DocumentMut = cargo_text.parse()?;
    let deps = cargo_doc.entry("dependencies");
    match deps {
        toml_edit::Entry::Occupied(mut occupied_entry) => {
            let Some(deps_table) = occupied_entry.get_mut().as_table_mut() else {
                return Err(anyhow!("existing dependencies table is... not a table"));
            };
            if !deps_table.contains_key("wit-bindgen") {
                let wbg_ver = toml_edit::Formatted::new("0.34.0".to_owned());
                deps_table.insert("wit-bindgen", toml_edit::Item::Value(toml_edit::Value::String(wbg_ver)));
                did_change = true;
            }
        },
        toml_edit::Entry::Vacant(vacant_entry) => {
            let mut deps_table = toml_edit::Table::new();
            let wbg_ver = toml_edit::Formatted::new("0.34.0".to_owned());
            deps_table.insert("wit-bindgen", toml_edit::Item::Value(toml_edit::Value::String(wbg_ver)));
            vacant_entry.insert(toml_edit::Item::Table(deps_table));
            did_change = true;
        },
    };
    let new_cargo_text = cargo_doc.to_string();
    if did_change {
        std::fs::write(cargo_toml, new_cargo_text)?;
    }

    // inject or modify bind script in src/lib.rs
    let lib_file = root_dir.join("src/lib.rs");
    if !lib_file.is_file() {
        bail!("src/lib.rs is not a file");
    }
    let lib_text = std::fs::read_to_string(&lib_file)?;

    // ALL RIGHT HERE WE GO

    // If we already have a `mod deps`...
    if let Some(mod_deps_index) = lib_text.lines().position(|l| l.trim().starts_with("mod deps {")) {
        // oh no we gotta do some flippin parsing
        // TODO: can syn help us?  It seemed a bit agonising and not terribly supportive
        let mut lines: Vec<_> = lib_text.lines().map(|s| s.to_owned()).collect();
        let mut index = mod_deps_index;
        let mut in_imports = false;
        let mut in_with = false;
        let mut unseen_imports: Vec<_> = interfaces.iter().map(|i| format!("import {i};")).collect();
        let mut unseen_withs: Vec<_> = interfaces.iter().map(|i| format!("\"{i}\": generate,")).collect();
        loop {
            index += 1;
            let current = &lines[index];
            if current.trim().starts_with("world imports {") {
                in_imports = true;
                continue;
            }
            if in_imports {
                if current.trim().starts_with("}") {
                    // insert those not yet seen and BUMP INDEX PAST THEM
                    in_imports = false;
                    for import in &unseen_imports {
                        lines.insert(index - 1, format!("            {import}"));
                        index += 1;
                    }
                    continue;;
                }
                if current.trim().starts_with("import ") {
                    // if this was one we were planning to insert, remove it from the plan!
                    unseen_imports.retain(|imp| imp != current.trim());
                    continue;
                }
            }
            if current.trim().starts_with("with: {") {
                in_with = true;
                continue;
            }
            if in_with {
                if current.trim().ends_with(": generate,") {
                    // if this was one we were planning to insert, remove it from the plan!
                    unseen_withs.retain(|w| w != current.trim());
                    continue;
                }

            }
        }

    } else {
        // We will create a `mod deps` with SCIENCE in it
        let imps = interfaces.iter().map(|i| format!(r#"            import {i};"#)).collect::<Vec<_>>();
        let imps = imps.join("\n");
        let gens = interfaces.iter().map(|i| format!(r#"            "{i}": generate,"#)).collect::<Vec<_>>();
        let gens = gens.join("\n");
        let deps_text = format!(r###"
mod deps {{
    wit_bindgen::generate!({{
        inline: r#"
        package root:component;
        world imports {{
{imps}
        }}
        "#,
        with: {{
{gens}
        }},
        path: ".wit/components/{component_id}",
    }});
}}
"###);

        // TODO: insert this into the file in a SCIENTIFICALLY DETERMINED place
    }

    todo!()
}
