// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use deno_core::anyhow::bail;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::futures::stream::FuturesOrdered;
use deno_core::futures::StreamExt;
use deno_core::parking_lot::Mutex;
use deno_npm::registry::NpmRegistryApi;
use deno_npm::resolution::NpmResolutionSnapshot;
use deno_npm::resolution::NpmResolutionSnapshotCreateOptions;
use deno_npm::resolution::NpmResolutionSnapshotCreateOptionsPackage;
use deno_npm::NpmPackageId;
use deno_semver::npm::NpmPackageReq;

use crate::args::config_file::LockConfig;
use crate::args::ConfigFile;
use crate::Flags;

use super::DenoSubcommand;

pub use deno_lockfile::Lockfile;
pub use deno_lockfile::LockfileError;

pub fn discover(
  flags: &Flags,
  maybe_config_file: Option<&ConfigFile>,
) -> Result<Option<Lockfile>, AnyError> {
  if flags.no_lock
    || matches!(
      flags.subcommand,
      DenoSubcommand::Install(_) | DenoSubcommand::Uninstall(_)
    )
  {
    return Ok(None);
  }

  let filename = match flags.lock {
    Some(ref lock) => PathBuf::from(lock),
    None => match maybe_config_file {
      Some(config_file) => {
        if config_file.specifier.scheme() == "file" {
          match config_file.to_lock_config()? {
            Some(LockConfig::Bool(lock)) if !lock => {
              return Ok(None);
            }
            Some(LockConfig::PathBuf(lock)) => config_file
              .specifier
              .to_file_path()
              .unwrap()
              .parent()
              .unwrap()
              .join(lock),
            _ => {
              let mut path = config_file.specifier.to_file_path().unwrap();
              path.set_file_name("deno.lock");
              path
            }
          }
        } else {
          return Ok(None);
        }
      }
      None => return Ok(None),
    },
  };

  let lockfile = Lockfile::new(filename, flags.lock_write)?;
  Ok(Some(lockfile))
}

pub async fn snapshot_from_lockfile(
  lockfile: Arc<Mutex<Lockfile>>,
  api: &dyn NpmRegistryApi,
) -> Result<NpmResolutionSnapshot, AnyError> {
  let (root_packages, mut packages) = {
    let lockfile = lockfile.lock();

    let mut root_packages =
      HashMap::<NpmPackageReq, NpmPackageId>::with_capacity(
        lockfile.content.npm.specifiers.len(),
      );
    // collect the specifiers to version mappings
    for (key, value) in &lockfile.content.npm.specifiers {
      let package_req = NpmPackageReq::from_str(key)
        .with_context(|| format!("Unable to parse npm specifier: {key}"))?;
      let package_id = NpmPackageId::from_serialized(value)?;
      root_packages.insert(package_req, package_id.clone());
    }

    // now fill the packages except for the dist information
    let mut packages = Vec::with_capacity(lockfile.content.npm.packages.len());
    for (key, package) in &lockfile.content.npm.packages {
      let pkg_id = NpmPackageId::from_serialized(key)?;

      // collect the dependencies
      let mut dependencies = HashMap::with_capacity(package.dependencies.len());
      for (name, specifier) in &package.dependencies {
        let dep_id = NpmPackageId::from_serialized(specifier)?;
        dependencies.insert(name.clone(), dep_id);
      }

      packages.push(NpmResolutionSnapshotCreateOptionsPackage {
        pkg_id,
        dist: Default::default(), // temporarily empty
        dependencies,
      });
    }
    (root_packages, packages)
  };

  // now that the lockfile is dropped, fetch the package version information
  let mut version_infos =
      FuturesOrdered::from_iter(packages.iter().map(|p| p.pkg_id.nv.clone()).map(
        |nv| async move {
          let package_info = api.package_info(&nv.name).await?;
          match package_info.version_info(&nv) {
            Ok(version_info) => Ok(version_info),
            Err(err) => {
              bail!("Could not find '{}' specified in the lockfile. Maybe try again with --reload", err.0);
            }
          }
        },
      ));

  let mut i = 0;
  while let Some(version_info) = version_infos.next().await {
    packages[i].dist = version_info?.dist;
    i += 1;
  }

  NpmResolutionSnapshot::from_packages(NpmResolutionSnapshotCreateOptions {
    packages,
    root_packages,
  })
  .context("The lockfile is corrupt. You can recreate it with --lock-write")
}
