use anyhow::{Context, Result};
use wit_component::WitPrinter;
use wit_parser::{PackageId, Resolve};

/// Converts a Resolve object to WIT content.
pub fn resolve_to_wit(resolve: &Resolve, package_id: PackageId) -> Result<String> {
    let mut printer = WitPrinter::default();
    printer.emit_docs(false);

    let ids = resolve
        .packages
        .iter()
        .map(|(id, _)| id)
        .filter(|id| *id != package_id)
        .collect::<Vec<_>>();

    printer.print(resolve, package_id, &ids)?;
    Ok(printer.output.to_string())
}

pub fn parse_component_bytes(bytes: Vec<u8>) -> Result<(Resolve, PackageId)> {
    wasmparser::validate(&bytes)
        .context("Provided component does not seem to be a valid component")?;

    let decoded_wasm = wit_component::decode(&bytes)?;
    let resolve = decoded_wasm.resolve().clone();
    let main = decoded_wasm.package();

    Ok((resolve, main))
}

/// Retrieves the exported interfaces from the resolved world.
pub fn get_exported_interfaces(
    resolve: &Resolve,
    world_id: wit_parser::WorldId,
) -> Vec<(wit_parser::PackageName, String)> {
    resolve.worlds[world_id]
        .exports
        .iter()
        .filter_map(|(_k, v)| match v {
            wit_parser::WorldItem::Interface { id, .. } => {
                let i = &resolve.interfaces[*id];
                let pkg_id = i.package.unwrap();
                let pkg = &resolve.packages[pkg_id];
                Some((pkg.name.clone(), i.name.clone().unwrap_or_default()))
            }
            _ => None,
        })
        .collect()
}

// pub fn merge_dependecy_package(
//     base_resolve_file: Option<&PathBuf>,
//     dependency_resolve: &Resolve,
//     dependency_pkg_id: PackageId,
// ) -> Result<(Resolve, PackageId)> {
//     let mut base_resolve = Resolve::default();
//     let base_resolve_pkg_id = match base_resolve_file {
//         Some(path) => base_resolve.push_file(path)?,
//         None => base_resolve.push_str("base_resolve.wit", DEFAULT_WIT)?,
//     };
//     let base_resolve_world_id = base_resolve.select_world(base_resolve_pkg_id, Some("deps"))?;

//     let dependecy_main_world_id =
//         dependency_resolve.select_world(dependency_pkg_id, Some("dependency-world"))?;
//     let remap = base_resolve.merge(dependency_resolve.clone())?;
//     let dependecy_world_id = remap.map_world(dependecy_main_world_id, None)?;
//     base_resolve.merge_worlds(dependecy_world_id, base_resolve_world_id)?;

//     Ok((base_resolve, base_resolve_pkg_id))
// }
