// Copyright 2018 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use cargo::util::CfgExpr;
use context::BuildDependency;
use context::BuildTarget;
use context::CrateContext;
use context::LicenseData;
use context::WorkspaceContext;
use error::RazeErr;
use error::RazeResult;
use license;
use metadata::Metadata;
use metadata::Package;
use metadata::PackageId;
use metadata::testing as metadata_testing;
use serde_json;
use settings::CrateSettings;
use settings::GenMode;
use settings::RazeSettings;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::str::FromStr;
use tempdir::TempDir;
use util;

/**
 * An entity that can retrive deserialized metadata for a Cargo Workspace.
 *
 * The "CargoSubcommandMetadataFetcher" is probably the one you want.
 */
pub trait MetadataFetcher {
  fn fetch_metadata(&mut self, files: CargoWorkspaceFiles) -> RazeResult<Metadata>;
}

pub trait BuildPlanner {
  fn plan_build(
    &mut self,
    settings: &RazeSettings,
    files: CargoWorkspaceFiles,
  ) -> RazeResult<PlannedBuild>;
}

pub struct CargoWorkspaceFiles {
  pub toml_path: PathBuf,
  pub lock_path_opt: Option<PathBuf>,
}

pub struct BuildPlannerImpl {
  metadata_fetcher: Box<MetadataFetcher>,
}

pub struct PlannedBuild {
  pub workspace_context: WorkspaceContext,
  pub crate_contexts: Vec<CrateContext>,
}

pub struct CargoSubcommandMetadataFetcher;

impl BuildPlanner for BuildPlannerImpl {
  fn plan_build(
    &mut self,
    settings: &RazeSettings,
    files: CargoWorkspaceFiles,
  ) -> RazeResult<PlannedBuild> {
    let metadata = try!(self.metadata_fetcher.fetch_metadata(files));
    if settings.genmode == GenMode::Vendored {
      self.assert_crates_vendored(&metadata);
    }
    let crate_contexts = try!(self.produce_crate_contexts(&settings, &metadata));

    // TODO(acmcarther) :Stub
    Ok(PlannedBuild {
      crate_contexts: Vec::new(),
      workspace_context: WorkspaceContext {
        workspace_path: String::new(),
        platform_triple: String::new(),
        gen_workspace_prefix: String::new(),
      },
    })
  }
}

impl BuildPlannerImpl {
  pub fn new(metadata_fetcher: Box<MetadataFetcher>) -> BuildPlannerImpl {
    BuildPlannerImpl {
      metadata_fetcher: metadata_fetcher,
    }
  }

  fn assert_crates_vendored(&self, metadata: &Metadata) {
    for package in metadata.packages.iter() {
      let full_name = format!("{}-{}", package.name, package.version);
      let path = format!("./vendor/{}/", full_name);

      if fs::metadata(&path).is_err() {
        panic!(format!(
          "failed to find {}. Either switch to \"Remote\" genmode, or run `cargo vendor -x` \
           first.",
          &path
        ));
      };
    }
  }

  fn get_root_deps(&self, metadata: &Metadata) -> RazeResult<Vec<PackageId>> {
    let root_resolve_node_opt = {
      let root_id = &metadata.resolve.root;
      metadata
        .resolve
        .nodes
        .iter()
        .find(|node| &node.id == root_id)
    };
    let root_resolve_node = if root_resolve_node_opt.is_some() {
      // UNWRAP: Guarded above
      root_resolve_node_opt.unwrap()
    } else {
      return Err(build_planner_err("Finding root crate details"));
    };
    Ok(root_resolve_node.dependencies.clone())
  }

  fn produce_crate_contexts(
    &self,
    settings: &RazeSettings,
    metadata: &Metadata,
  ) -> RazeResult<Vec<CrateContext>> {
    let root_direct_deps = try!(self.get_root_deps(&metadata));
    let packages_by_id = metadata
      .packages
      .iter()
      .map(|p| (p.id.clone(), p.clone()))
      .collect::<HashMap<PackageId, Package>>();

    // Verify that all nodes are present in package list
    {
      let mut missing_nodes = Vec::new();
      for node in metadata.resolve.nodes.iter() {
        if !packages_by_id.contains_key(&node.id) {
          missing_nodes.push(&node.id);
        }
      }
      if !missing_nodes.is_empty() {
        // This implies that we either have a mistaken understanding of Cargo resolution, or that
        // it broke.
        return Err(build_planner_err(&format!(
          "Metadata.packages list was missing keys: {:?}",
          missing_nodes
        )));
      }
    }

    let mut crate_contexts = Vec::new();
    // TODO(acmcarther): handle unwrap
    let platform_attrs = util::fetch_attrs(&settings.target).unwrap();
    for node in metadata.resolve.nodes.iter() {
      let own_package = packages_by_id.get(&node.id).unwrap();
      let full_name = format!("{}-{}", own_package.name, own_package.version);
      let path = format!("./vendor/{}/", full_name);

      // Skip the root package (which is probably a junk package, by convention)
      if own_package.id == metadata.resolve.root {
        continue;
      }

      // Resolve dependencies into types
      let mut build_dep_names = Vec::new();
      let mut dev_dep_names = Vec::new();
      let mut normal_dep_names = Vec::new();
      for dep in own_package.dependencies.iter() {
        if dep.target.is_some() {
          // UNWRAP: Safe from above check
          let target_str = dep.target.as_ref().unwrap();
          let target_cfg_expr: CfgExpr = FromStr::from_str(target_str).unwrap();

          // Skip this dep if it doesn't match our platform attributes
          if !target_cfg_expr.matches(&platform_attrs) {
            continue;
          }
        }

        match dep.kind.as_ref().map(|v| v.as_str()) {
          None | Some("normal") => normal_dep_names.push(dep.name.clone()),
          Some("dev") => dev_dep_names.push(dep.name.clone()),
          Some("build") => build_dep_names.push(dep.name.clone()),
          something_else => panic!(
            "Unhandlable dependency type {:?} for {} on {} detected!",
            something_else,
            own_package.name,
            dep.name
          ),
        }
      }

      let mut build_deps = Vec::new();
      let mut dev_deps = Vec::new();
      let mut normal_deps = Vec::new();
      for dep_id in node.dependencies.iter() {
        // UNWRAP: Safe from verification of packages_by_id
        let dep_package = packages_by_id.get(dep_id.as_str()).unwrap();
        let build_dependency = BuildDependency {
          name: dep_package.name.clone(),
          version: dep_package.version.clone(),
        };
        if build_dep_names.contains(&dep_package.name) {
          build_deps.push(build_dependency.clone());
        }

        if dev_dep_names.contains(&dep_package.name) {
          dev_deps.push(build_dependency.clone());
        }

        if normal_dep_names.contains(&dep_package.name) {
          normal_deps.push(build_dependency);
        }
      }
      build_deps.sort();
      dev_deps.sort();
      normal_deps.sort();

      let mut targets = try!(self.produce_targets(&own_package));
      targets.sort();

      let possible_crate_settings = settings
        .crates
        .get(&own_package.name)
        .and_then(|c| c.get(&own_package.version));

      let should_gen_buildrs = possible_crate_settings
        .map(|s| s.gen_buildrs.clone())
        .unwrap_or(false);
      let build_script_target = if should_gen_buildrs {
        targets
          .iter()
          .find(|t| t.kind.as_str() == "custom-build")
          .cloned()
      } else {
        None
      };

      let targets_sans_build_script = targets
        .into_iter()
        .filter(|t| t.kind.as_str() != "custom-build")
        .collect::<Vec<_>>();

      let additional_deps = possible_crate_settings
        .map(|s| s.additional_deps.clone())
        .unwrap_or(Vec::new());

      let additional_flags = possible_crate_settings
        .map(|s| s.additional_flags.clone())
        .unwrap_or(Vec::new());

      let extra_aliased_targets = possible_crate_settings
        .map(|s| s.extra_aliased_targets.clone())
        .unwrap_or(Vec::new());

      // Skip generated dependencies explicitly designated to be skipped (potentially due to
      // being replaced or customized as part of additional_deps)
      let non_skipped_normal_deps = possible_crate_settings
        .map(|s| prune_skipped_deps(&normal_deps, s))
        .unwrap_or_else(|| normal_deps);
      let non_skipped_build_deps = possible_crate_settings
        .map(|s| prune_skipped_deps(&build_deps, s))
        .unwrap_or_else(|| build_deps);

      let license_str = own_package
        .license
        .as_ref()
        .map(|s| s.as_str())
        .unwrap_or("");
      let licenses = load_and_dedup_licenses(license_str);

      // TODO(acmcarther): https://github.com/rust-lang/cargo/pull/5122
      let mut features = Vec::new();

      crate_contexts.push(CrateContext {
        pkg_name: own_package.name.clone(),
        pkg_version: own_package.version.clone(),
        licenses: licenses,
        features: features,
        is_root_dependency: root_direct_deps.contains(&node.id),
        metadeps: Vec::new(), /* TODO(acmcarther) */
        dependencies: non_skipped_normal_deps,
        build_dependencies: non_skipped_build_deps,
        dev_dependencies: dev_deps,
        path: path,
        build_script_target: build_script_target,
        targets: targets_sans_build_script,
        platform_triple: settings.target.to_owned(),
        additional_deps: additional_deps,
        additional_flags: additional_flags,
        extra_aliased_targets: extra_aliased_targets,
      })
    }

    Ok(crate_contexts)
  }

  fn produce_targets(&self, package: &Package) -> RazeResult<Vec<BuildTarget>> {
    let full_name = format!("{}-{}", package.name, package.version);
    let partial_path = format!("{}/", full_name);
    let partial_path_byte_length = partial_path.as_bytes().len();
    let mut targets = Vec::new();
    for target in package.targets.iter() {
      // N.B. This error is really weird, but it boils down to us not being able to find the crate's
      // name as part of the complete path to the crate root.
      // For example, "/some/long/path/crate-version/lib.rs" should contain crate-version in the path
      // for crate at some version.
      let crate_name_str_idx = try!(target.src_path.find(&partial_path).ok_or(
        build_planner_err(&format!(
          "{} had a target {} whose crate root appeared to be outside of the crate.",
          &full_name,
          target.name
        ))
      ));


      let local_path_bytes = target
        .src_path
        .bytes()
        .skip(crate_name_str_idx + partial_path_byte_length)
        .collect::<Vec<_>>();
      // UNWRAP: Sliced from a known unicode string -- conversion is safe
      let mut local_path_str = String::from_utf8(local_path_bytes).unwrap();
      if local_path_str.starts_with("./") {
        local_path_str = local_path_str.split_off(2);
      }

      for kind in target.kind.iter() {
        targets.push(BuildTarget {
          name: target.name.clone(),
          path: local_path_str.clone(),
          kind: kind.clone(),
        });
      }
    }

    Ok(targets)
  }
}

impl MetadataFetcher for CargoSubcommandMetadataFetcher {
  fn fetch_metadata(&mut self, files: CargoWorkspaceFiles) -> RazeResult<Metadata> {
    assert!(files.toml_path.is_file());
    assert!(
      files
        .lock_path_opt
        .as_ref()
        .map(|p| p.is_file())
        .unwrap_or(true)
    );

    // Copy files into a temp directory
    // UNWRAP: Guarded by function assertion
    let cargo_tempdir = {
      let dir = try!(
        TempDir::new("cargo_raze_metadata_dir")
          .map_err(|_| metadata_fetcher_err("creating tempdir"))
      );
      {
        let dir_path = dir.path();
        let new_toml_path = dir_path.join(files.toml_path.file_name().unwrap());
        try!(
          fs::copy(files.toml_path, new_toml_path)
            .map_err(|_| metadata_fetcher_err("copying cargo toml"))
        );
        if let Some(lock_path) = files.lock_path_opt {
          let new_lock_path = dir_path.join(lock_path.file_name().unwrap());
          try!(
            fs::copy(lock_path, new_lock_path)
              .map_err(|_| metadata_fetcher_err("copying cargo lock"))
          );
        }
      }
      dir
    };

    // Shell out to cargo
    let exec_output = try!(
      Command::new("cargo")
        .current_dir(cargo_tempdir.path())
        .args(&["metadata", "--format-version", "1"])
        .output()
        .map_err(|_| metadata_fetcher_err("running `cargo metadata`"))
    );

    // Handle command errs
    let stdout_str =
      String::from_utf8(exec_output.stdout).unwrap_or("[unparsable bytes]".to_owned());
    if !exec_output.status.success() {
      let stderr_str =
        String::from_utf8(exec_output.stderr).unwrap_or("[unparsable bytes]".to_owned());
      println!("`cargo metadata` failed. Inspect Cargo.toml for issues!");
      println!("stdout: {}", stdout_str);
      println!("stderr: {}", stderr_str);
      return Err(metadata_fetcher_err("running `cargo metadata`"));
    }

    // Parse and yield metadata
    serde_json::from_str::<Metadata>(&stdout_str)
      .map_err(|_| metadata_fetcher_err("parsing `cargo metadata` output"))
  }
}

fn prune_skipped_deps(
  deps: &Vec<BuildDependency>,
  crate_settings: &CrateSettings,
) -> Vec<BuildDependency> {
  deps
    .iter()
    .filter(|d| {
      !crate_settings
        .skipped_deps
        .contains(&format!("{}-{}", d.name, d.version))
    })
    .map(|dep| dep.clone())
    .collect::<Vec<_>>()
}

fn load_and_dedup_licenses(licenses: &str) -> Vec<LicenseData> {
  let mut rating_to_license_name = HashMap::new();
  for (license_name, license_type) in license::get_available_licenses(licenses) {
    let rating = license_type.to_bazel_rating();

    if rating_to_license_name.contains_key(&rating) {
      let mut license_names_str: &mut String = rating_to_license_name.get_mut(&rating).unwrap();
      license_names_str.push_str(",");
      license_names_str.push_str(&license_name);
    } else {
      rating_to_license_name.insert(rating, license_name.to_owned());
    }
  }

  let mut license_data_list = rating_to_license_name
    .into_iter()
    .map(|(rating, name)| {
      LicenseData {
        name: name,
        rating: rating.to_owned(),
      }
    })
    .collect::<Vec<_>>();

  // Make output deterministic
  license_data_list.sort_by_key(|d| d.rating.clone());

  license_data_list
}

fn build_planner_err(message: &str) -> RazeErr {
  RazeErr {
    component: "BuildPlanner".to_owned(),
    message: message.to_owned(),
  }
}


fn metadata_fetcher_err(message: &str) -> RazeErr {
  RazeErr {
    component: "MetadataFetcher".to_owned(),
    message: message.to_owned(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs::File;
  use std::io::Write;

  fn basic_toml() -> &'static str {
    "
[package]
name = \"test\"
version = \"0.0.1\"

[lib]
path = \"not_a_file.rs\"
    "
  }

  fn basic_lock() -> &'static str {
    "
[[package]]
name = \"test\"
version = \"0.0.1\"
dependencies = [
]
    "
  }

  //#[test]
  fn test_cargo_subcommand_metadata_fetcher_works_without_lock() {
    let dir = TempDir::new("test_cargo_raze_metadata_dir").unwrap();
    let toml_path = dir.path().join("Cargo.toml");
    let mut toml = File::create(&toml_path).unwrap();
    toml.write_all(basic_toml().as_bytes()).unwrap();
    let files = CargoWorkspaceFiles {
      toml_path: toml_path,
      lock_path_opt: None,
    };

    let mut fetcher = CargoSubcommandMetadataFetcher;

    fetcher.fetch_metadata(files).unwrap();
  }

  //#[test]
  fn test_cargo_subcommand_metadata_fetcher_works_with_lock() {
    let dir = TempDir::new("test_cargo_raze_metadata_dir").unwrap();
    let toml_path = {
      let path = dir.path().join("Cargo.toml");
      let mut toml = File::create(&path).unwrap();
      toml.write_all(basic_toml().as_bytes()).unwrap();
      path
    };
    let lock_path = {
      let path = dir.path().join("Cargo.lock");
      let mut lock = File::create(&path).unwrap();
      lock.write_all(basic_lock().as_bytes()).unwrap();
      path
    };
    let files = CargoWorkspaceFiles {
      toml_path: toml_path,
      lock_path_opt: Some(lock_path),
    };

    let mut fetcher = CargoSubcommandMetadataFetcher;

    fetcher.fetch_metadata(files).unwrap();
  }

  //#[test]
  fn test_cargo_subcommand_metadata_fetcher_handles_bad_files() {
    let dir = TempDir::new("test_cargo_raze_metadata_dir").unwrap();
    let toml_path = {
      let path = dir.path().join("Cargo.toml");
      let mut toml = File::create(&path).unwrap();
      toml.write_all(b"hello").unwrap();
      path
    };
    let files = CargoWorkspaceFiles {
      toml_path: toml_path,
      lock_path_opt: None,
    };

    let mut fetcher = CargoSubcommandMetadataFetcher;
    assert!(fetcher.fetch_metadata(files).is_err());
  }

}
