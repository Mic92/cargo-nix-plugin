//! Construct Nix values from resolved workspace data.
//!
//! Uses `NixContext` + the `nix_attrs!` macro to build attrsets and lists
//! directly via the Nix C API, without JSON serialization.

use crate::nix_attrs;
use crate::nix_val::{NixContext, Result, Value};
use crate::resolve::{BinTarget, CrateInfo, DepInfo, SourceInfo, WorkspaceResult};

use std::collections::BTreeMap;

pub fn mk_workspace_result(nx: &NixContext, ws: &WorkspaceResult) -> Result<Value> {
    nix_attrs!(nx,
        "root"             => nx.mk_opt_string(&ws.root)?,
        "workspaceRoot"    => nx.mk_string(&ws.workspace_root)?,
        "workspaceMembers" => nx.mk_string_map(&ws.workspace_members)?,
        "crates"           => mk_crates_map(nx, &ws.crates)?,
    )
}

fn mk_crates_map(nx: &NixContext, crates: &BTreeMap<String, CrateInfo>) -> Result<Value> {
    let entries: Vec<_> = crates
        .iter()
        .map(|(k, v)| mk_crate_info(nx, v).map(|val| (k.as_str(), val)))
        .collect::<Result<_>>()?;
    nx.mk_attrs(entries)
}

fn mk_crate_info(nx: &NixContext, c: &CrateInfo) -> Result<Value> {
    nix_attrs!(nx,
        "crateName"              => nx.mk_string(&c.crate_name)?,
        "version"                => nx.mk_string(&c.version)?,
        "edition"                => nx.mk_string(&c.edition)?,
        "sha256"                 => nx.mk_opt_string(&c.sha256)?,
        "source"                 => mk_source_info(nx, &c.source)?,
        "dependencies"           => mk_dep_list(nx, &c.dependencies)?,
        "buildDependencies"      => mk_dep_list(nx, &c.build_dependencies)?,
        "devDependencies"        => mk_dep_list(nx, &c.dev_dependencies)?,
        "features"               => nx.mk_string_list_map(&c.features)?,
        "resolvedDefaultFeatures"=> nx.mk_string_list(&c.resolved_default_features)?,
        "procMacro"              => nx.mk_bool(c.proc_macro)?,
        "build"                  => nx.mk_opt_string(&c.build)?,
        "libPath"                => nx.mk_opt_string(&c.lib_path)?,
        "libName"                => nx.mk_opt_string(&c.lib_name)?,
        "crateBin"               => mk_bin_list(nx, &c.crate_bin)?,
        "libCrateTypes"          => nx.mk_string_list(&c.lib_crate_types)?,
        "links"                  => nx.mk_opt_string(&c.links)?,
        "authors"                => nx.mk_string_list(&c.authors)?,
    )
}

fn mk_source_info(nx: &NixContext, src: &Option<SourceInfo>) -> Result<Value> {
    match src {
        None => nx.mk_null(),
        Some(SourceInfo::CratesIo) => nix_attrs!(nx, "type" => nx.mk_string("crates-io")?),
        Some(SourceInfo::Local { path }) => nix_attrs!(nx,
            "type" => nx.mk_string("local")?,
            "path" => nx.mk_string(path)?,
        ),
        Some(SourceInfo::Git { url, rev }) => nix_attrs!(nx,
            "type" => nx.mk_string("git")?,
            "url"  => nx.mk_string(url)?,
            "rev"  => nx.mk_string(rev)?,
        ),
    }
}

fn mk_dep_info(nx: &NixContext, d: &DepInfo) -> Result<Value> {
    nix_attrs!(nx,
        "name"               => nx.mk_string(&d.name)?,
        "packageId"          => nx.mk_string(&d.package_id)?,
        "rename"             => nx.mk_opt_string(&d.rename)?,
        "usesDefaultFeatures"=> nx.mk_bool(d.uses_default_features)?,
        "features"           => nx.mk_string_list(&d.features)?,
    )
}

fn mk_dep_list(nx: &NixContext, deps: &[DepInfo]) -> Result<Value> {
    let items: Vec<_> = deps
        .iter()
        .map(|d| mk_dep_info(nx, d))
        .collect::<Result<_>>()?;
    nx.mk_list(items)
}

fn mk_bin_list(nx: &NixContext, bins: &[BinTarget]) -> Result<Value> {
    let items: Vec<_> = bins
        .iter()
        .map(|b| nix_attrs!(nx, "name" => nx.mk_string(&b.name)?, "path" => nx.mk_string(&b.path)?))
        .collect::<Result<_>>()?;
    nx.mk_list(items)
}
